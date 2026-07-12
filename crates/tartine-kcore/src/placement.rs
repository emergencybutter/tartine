//! Rendezvous (HRW) placement — the kernel-safe implementation. This is
//! the *only* implementation of the algorithm in the whole workspace:
//! `tartine-core::placement` (used by the FUSE prototype, DESIGN.md's
//! "prototype first" path) is a thin `Vec`-based wrapper that calls
//! straight into `hrw_select` below, specifically so the prototype and
//! the eventual kernel module are provably running the same placement
//! decisions, not two implementations that could drift apart.
//!
//! Weighting uses **logarithmic weighted rendezvous** (Thaler &
//! Ravishankar): `score = weight / -ln(hash-as-unit-interval)`, which
//! makes each disk's selection probability exactly proportional to its
//! weight. The obvious alternative — multiplying the hash by the weight —
//! is *biased*: for weights 2:1 it hands the heavy disk 75% of keys
//! instead of the proportional 66.7%, and the skew compounds as the pool
//! grows. Since `-ln u` and `-log2 u` differ only by a constant factor
//! that cancels in comparisons, the implementation uses a fixed-point
//! Q16 `-log2` (integer square-and-shift, no lookup tables).
//!
//! No allocation, no floating point (kernel code doesn't get an FPU for
//! free — see DESIGN.md's kernel-build notes), fixed upper bound on `n`
//! so the working set fits on the stack. The one `u128` division below
//! compiles to a `__udivti3` call supplied by `compiler_builtins`, which
//! the freestanding staticlib bundles — kernel *C* has no native 128-bit
//! divide, but this is Rust-side code with its own builtins.

/// One candidate disk, as the caller (C kernel module or the
/// `tartine-core` wrapper) already has it — this crate never owns a pool
/// map itself, it only scores/selects from what it's handed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DiskCandidate {
    pub disk_id_hi: u64,
    pub disk_id_lo: u64,
    /// Fixed-point weight, `real_weight * 1000`. Integer, not `f64`:
    /// kernel code avoids floating point (no implicit FPU state
    /// save/restore in most contexts), so this type is what the
    /// FFI boundary uses even though `tartine-core`'s convenience
    /// wrapper still takes a friendlier `f64` from callers and converts.
    /// Selection probability is proportional to this value (see the
    /// module doc comment).
    pub weight_milli: u32,
    /// 0/1 rather than `bool` — `bool`'s layout across an FFI boundary
    /// with a C caller is easiest to reason about as an explicit `u8`.
    pub active: u8,
}

// The C side (kernel/tartine_kcore.h) mirrors this struct by hand and
// carries the matching static_asserts; if either side's layout drifts,
// one of the two builds refuses to compile instead of corrupting memory.
const _: () = assert!(core::mem::size_of::<DiskCandidate>() == 24);
const _: () = assert!(core::mem::align_of::<DiskCandidate>() == 8);

/// Hard cap on replicas selectable in one call, so `hrw_select` can use a
/// fixed-size stack scratch buffer instead of allocating. Any realistic
/// replication factor (data or metadata) is far below this.
pub const MAX_SELECT: usize = 16;

/// `-log2(u)` in Q16 fixed point, where `u = (h + 1) / 2^64 ∈ (0, 1]`.
/// Range: 0 (h = u64::MAX, u = 1) up to `64 << 16` (h = 0). The
/// fractional bits come from 16 rounds of square-and-shift binary
/// logarithm — exact to the last bit, no polynomial approximation, no
/// tables.
fn neg_log2_q16(h: u64) -> u64 {
    let x = match h.checked_add(1) {
        Some(x) => x,
        None => return 0, // u == 1 exactly, -log2(1) = 0
    };
    let int_part = 63 - x.leading_zeros() as u64; // floor(log2 x)

    // Mantissa in Q32, value in [1, 2).
    let mut m: u64 = if int_part >= 32 {
        x >> (int_part - 32)
    } else {
        x << (32 - int_part)
    };
    let mut frac: u64 = 0;
    for _ in 0..16 {
        // v in [1, 2) as Q32; v^2 in [1, 4) as Q64. If v^2 >= 2, the next
        // log2 bit is 1 and we renormalize by halving.
        let sq = (m as u128) * (m as u128);
        frac <<= 1;
        if sq >= 1u128 << 65 {
            frac |= 1;
            m = (sq >> 33) as u64;
        } else {
            m = (sq >> 32) as u64;
        }
    }
    (64 << 16) - ((int_part << 16) | frac)
}

/// Logarithmic-rendezvous score: `weight / -log2(u)`, as a `u128` so the
/// division keeps 64 bits of precision even at the extremes (`u → 1`
/// makes the denominator tiny and the score huge). The `+ 1` on the
/// denominator (one Q16 ulp) avoids division by zero at `u == 1` and
/// shifts every candidate equally, so proportionality is unaffected.
pub fn hrw_score(disk_id_hi: u64, disk_id_lo: u64, key: u64, weight_milli: u32) -> u128 {
    let id = ((disk_id_hi as u128) << 64) | disk_id_lo as u128;
    let raw = crate::hash::hash_u128_u64(id, key);
    score_from(raw, weight_milli)
}

fn score_from(raw: u64, weight_milli: u32) -> u128 {
    let denom = neg_log2_q16(raw) + 1;
    ((core::cmp::max(weight_milli, 1) as u128) << 64) / (denom as u128)
}

/// One scored candidate during selection. `raw` and the disk id are
/// tie-breakers so the result is fully deterministic regardless of the
/// order the caller happened to list candidates in (the FUSE prototype
/// iterates a HashMap, whose order varies run to run).
#[derive(Clone, Copy)]
struct Ranked {
    score: u128,
    raw: u64,
    id_hi: u64,
    id_lo: u64,
    idx: u32,
}

fn beats(a: &Ranked, b: &Ranked) -> bool {
    (a.score, a.raw, a.id_hi, a.id_lo) > (b.score, b.raw, b.id_hi, b.id_lo)
}

/// Selects up to `out.len()` (capped at `MAX_SELECT`) active candidates
/// for `key`, highest score first, writing their *index into
/// `candidates`* to `out`. Returns the number written — fewer than
/// requested if there aren't enough active candidates, which callers
/// treat as "under-replicated, repair when possible" rather than an
/// error (DESIGN.md §7.4).
pub fn hrw_select(candidates: &[DiskCandidate], key: u64, out: &mut [u32]) -> usize {
    let want = core::cmp::min(out.len(), MAX_SELECT);
    if want == 0 {
        return 0;
    }

    let zero = Ranked {
        score: 0,
        raw: 0,
        id_hi: 0,
        id_lo: 0,
        idx: 0,
    };
    let mut top = [zero; MAX_SELECT];
    let mut filled = 0usize;

    for (idx, c) in candidates.iter().enumerate() {
        if c.active == 0 {
            continue;
        }
        let id = ((c.disk_id_hi as u128) << 64) | c.disk_id_lo as u128;
        let raw = crate::hash::hash_u128_u64(id, key);
        let entry = Ranked {
            score: score_from(raw, c.weight_milli),
            raw,
            id_hi: c.disk_id_hi,
            id_lo: c.disk_id_lo,
            idx: idx as u32,
        };

        if filled < want {
            let mut pos = filled;
            while pos > 0 && beats(&entry, &top[pos - 1]) {
                top[pos] = top[pos - 1];
                pos -= 1;
            }
            top[pos] = entry;
            filled += 1;
        } else if beats(&entry, &top[want - 1]) {
            let mut pos = want - 1;
            while pos > 0 && beats(&entry, &top[pos - 1]) {
                top[pos] = top[pos - 1];
                pos -= 1;
            }
            top[pos] = entry;
        }
    }

    for (i, slot) in out.iter_mut().enumerate().take(filled) {
        *slot = top[i].idx;
    }
    filled
}

/// C ABI entry point. `candidates`/`out` are borrowed for the duration of
/// the call only; the kernel module owns and frees them. Returns the
/// number of entries written to `out` (see `hrw_select`), or 0 if either
/// pointer is null.
///
/// # Safety
/// `candidates` must point to `n_candidates` valid `DiskCandidate`s, and
/// `out` to `n_out` valid, writable `u32`s, for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn tartine_hrw_select(
    candidates: *const DiskCandidate,
    n_candidates: usize,
    key: u64,
    out: *mut u32,
    n_out: usize,
) -> usize {
    if candidates.is_null() || out.is_null() {
        return 0;
    }
    let candidates = core::slice::from_raw_parts(candidates, n_candidates);
    let out = core::slice::from_raw_parts_mut(out, n_out);
    hrw_select(candidates, key, out)
}

/// C ABI entry point for the placement key hash (`(inode, chunk_seq)` or
/// `(inode, extent_index)`), so the kernel module doesn't need its own
/// hash implementation to stay bit-compatible with this crate's scoring.
#[no_mangle]
pub extern "C" fn tartine_hash_key(inode: u64, seq: u64) -> u64 {
    crate::hash::hash_u128_u64(inode as u128, seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn candidates(n: usize) -> Vec<DiskCandidate> {
        (0..n)
            .map(|i| DiskCandidate {
                disk_id_hi: 0,
                disk_id_lo: i as u64 + 1,
                weight_milli: 1000,
                active: 1,
            })
            .collect()
    }

    #[test]
    fn neg_log2_q16_sanity() {
        assert_eq!(neg_log2_q16(u64::MAX), 0); // u = 1
        assert_eq!(neg_log2_q16(0), 64 << 16); // u = 2^-64
                                               // h + 1 = 2^63 → u = 1/2 → -log2(u) = 1.0 exactly
        assert_eq!(neg_log2_q16((1u64 << 63) - 1), 1 << 16);
    }

    #[test]
    fn picks_requested_count_no_duplicates() {
        let c = candidates(5);
        let mut out = [0u32; 3];
        let n = hrw_select(&c, 42, &mut out);
        assert_eq!(n, 3);
        let mut sorted = out.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3);
    }

    #[test]
    fn fewer_active_than_requested_returns_fewer() {
        let mut c = candidates(5);
        for cand in c.iter_mut().skip(2) {
            cand.active = 0;
        }
        let mut out = [0u32; 3];
        let n = hrw_select(&c, 7, &mut out);
        assert_eq!(n, 2);
    }

    #[test]
    fn adding_a_disk_moves_a_minority_of_keys() {
        let before = candidates(10);
        let mut after = before.clone();
        after.push(DiskCandidate {
            disk_id_hi: 0,
            disk_id_lo: 999,
            weight_milli: 1000,
            active: 1,
        });

        let mut changed = 0;
        for key in 0u64..1000 {
            let mut a = [0u32; 2];
            let mut b = [0u32; 2];
            hrw_select(&before, key, &mut a);
            hrw_select(&after, key, &mut b);
            if a != b {
                changed += 1;
            }
        }
        assert!(changed < 1000 / 3, "changed = {changed}");
    }

    #[test]
    fn weighted_selection_is_capacity_proportional() {
        // Weights 1:3 → the heavy disk should win 75% of keys under
        // logarithmic rendezvous. The multiply-by-weight scoring this
        // replaced gave it ~83% — this test exists to catch that class
        // of bias, so its tolerance band deliberately excludes 0.83.
        let c = vec![
            DiskCandidate {
                disk_id_hi: 0,
                disk_id_lo: 1,
                weight_milli: 1000,
                active: 1,
            },
            DiskCandidate {
                disk_id_hi: 0,
                disk_id_lo: 2,
                weight_milli: 3000,
                active: 1,
            },
        ];
        let trials = 20_000u64;
        let mut heavy = 0u64;
        for key in 0..trials {
            let mut out = [0u32; 1];
            assert_eq!(hrw_select(&c, key, &mut out), 1);
            if out[0] == 1 {
                heavy += 1;
            }
        }
        let frac = heavy as f64 / trials as f64;
        assert!(
            (0.72..=0.78).contains(&frac),
            "heavy-disk fraction = {frac}"
        );
    }

    #[test]
    fn equal_weights_select_uniformly() {
        let c = candidates(4);
        let trials = 20_000u64;
        let mut counts = [0u64; 4];
        for key in 0..trials {
            let mut out = [0u32; 1];
            hrw_select(&c, key, &mut out);
            counts[out[0] as usize] += 1;
        }
        for (i, &n) in counts.iter().enumerate() {
            let frac = n as f64 / trials as f64;
            assert!((0.22..=0.28).contains(&frac), "disk {i} fraction = {frac}");
        }
    }
}
