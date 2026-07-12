//! Rendezvous (HRW) placement — the kernel-safe implementation. This is
//! the *only* implementation of the algorithm in the whole workspace:
//! `tartine-core::placement` (used by the FUSE prototype, DESIGN.md's
//! "prototype first" path) is a thin `Vec`-based wrapper that calls
//! straight into this module, specifically so the prototype and the
//! eventual kernel module are provably running the same placement
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
//!
//! **Per-file redundancy policy** (DESIGN.md §10): a file doesn't just
//! have a replica *count*, it has a list of replica *slots*, each either
//! "any disk of this class" (HDD/SSD/NVMe) or "this specific disk"
//! (pinned). `place_redundancy` is the general placement entry point —
//! it walks the slot list once, excluding disks already chosen for an
//! earlier slot of the same file so replicas never collide. `hrw_select`
//! (the uniform "N any disks" case used by simple callers/tests) is now
//! implemented *in terms of* `hrw_select_one`, so there is exactly one
//! place the selection logic lives, and the pre-existing tests below
//! double as a proof the refactor didn't change behavior.

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
    /// This disk's actual class: `CLASS_HDD`/`CLASS_SSD`/`CLASS_NVME`.
    /// Never `CLASS_ANY` for a real candidate — that value only means
    /// something as a *slot constraint* (§10), not a disk's own class.
    pub class: u8,
}

// The C side (kernel/tartine_kcore.h) mirrors this struct by hand and
// carries the matching static_asserts; if either side's layout drifts,
// one of the two builds refuses to compile instead of corrupting memory.
// Adding `class` didn't grow the struct — it fills a byte that was
// already implicit alignment padding.
const _: () = assert!(core::mem::size_of::<DiskCandidate>() == 24);
const _: () = assert!(core::mem::align_of::<DiskCandidate>() == 8);

/// Slot constraint values, shared between `DiskCandidate::class` (where
/// `CLASS_ANY` never appears — a disk always has a concrete class) and
/// `ReplicaSlotSpec::required_class` (where it's the common case).
pub const CLASS_ANY: u8 = 0;
pub const CLASS_HDD: u8 = 1;
pub const CLASS_SSD: u8 = 2;
pub const CLASS_NVME: u8 = 3;

/// Hard cap on replicas selectable in one call, so selection can use a
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
fn score_from(raw: u64, weight_milli: u32) -> u128 {
    let denom = neg_log2_q16(raw) + 1;
    ((core::cmp::max(weight_milli, 1) as u128) << 64) / (denom as u128)
}

pub fn hrw_score(disk_id_hi: u64, disk_id_lo: u64, key: u64, weight_milli: u32) -> u128 {
    let id = ((disk_id_hi as u128) << 64) | disk_id_lo as u128;
    let raw = crate::hash::hash_u128_u64(id, key);
    score_from(raw, weight_milli)
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

fn rank(key: u64, idx: usize, c: &DiskCandidate) -> Ranked {
    let id = ((c.disk_id_hi as u128) << 64) | c.disk_id_lo as u128;
    let raw = crate::hash::hash_u128_u64(id, key);
    Ranked {
        score: score_from(raw, c.weight_milli),
        raw,
        id_hi: c.disk_id_hi,
        id_lo: c.disk_id_lo,
        idx: idx as u32,
    }
}

/// Selects the single highest-scoring `Active` candidate for `key` that
/// is not in `excluded` (indices already chosen for an earlier replica
/// slot of the same file) and, if `required_class != CLASS_ANY`, matches
/// it. Returns `None` if nothing qualifies — an unsatisfiable slot
/// (DESIGN.md §10: a hard error at policy-set time, ordinary
/// under-replication for an existing file whose class later ran out of
/// room).
pub fn hrw_select_one(
    candidates: &[DiskCandidate],
    key: u64,
    excluded: &[u32],
    required_class: u8,
) -> Option<u32> {
    let mut best: Option<Ranked> = None;
    for (idx, c) in candidates.iter().enumerate() {
        if c.active == 0 {
            continue;
        }
        if excluded.contains(&(idx as u32)) {
            continue;
        }
        if required_class != CLASS_ANY && c.class != required_class {
            continue;
        }
        let entry = rank(key, idx, c);
        if best.is_none_or(|b| beats(&entry, &b)) {
            best = Some(entry);
        }
    }
    best.map(|b| b.idx)
}

/// Selects up to `out.len()` (capped at `MAX_SELECT`) active candidates
/// for `key`, highest score first, writing their *index into
/// `candidates`* to `out`. Returns the number written — fewer than
/// requested if there aren't enough active candidates, which callers
/// treat as "under-replicated, repair when possible" rather than an
/// error (DESIGN.md §7.4). Equivalent to calling `hrw_select_one`
/// `out.len()` times, each excluding every index chosen so far — see the
/// module doc comment for why that equivalence matters.
pub fn hrw_select(candidates: &[DiskCandidate], key: u64, out: &mut [u32]) -> usize {
    let want = core::cmp::min(out.len(), MAX_SELECT);
    let mut excluded = [0u32; MAX_SELECT];
    let mut filled = 0usize;

    while filled < want {
        match hrw_select_one(candidates, key, &excluded[..filled], CLASS_ANY) {
            Some(idx) => {
                out[filled] = idx;
                excluded[filled] = idx;
                filled += 1;
            }
            None => break,
        }
    }
    filled
}

/// One entry in a file's redundancy policy (DESIGN.md §10): either "any
/// Active disk of `required_class`" (`CLASS_ANY` = no constraint) or, if
/// `pinned != 0`, exactly the disk named by `pinned_id_{hi,lo}` — HRW is
/// not consulted for a pinned slot, it's a direct lookup.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReplicaSlotSpec {
    pub required_class: u8,
    pub pinned: u8,
    pub _pad: [u8; 2],
    pub pinned_id_hi: u64,
    pub pinned_id_lo: u64,
}

const _: () = assert!(core::mem::size_of::<ReplicaSlotSpec>() == 24);
const _: () = assert!(core::mem::align_of::<ReplicaSlotSpec>() == 8);

/// Sentinel written to `out[i]` when slot `i` couldn't be satisfied
/// (pinned disk missing/inactive, or no remaining candidate of the
/// required class) — distinguishable from any real candidate index
/// since `candidates.len()` never approaches `u32::MAX`.
pub const UNPLACED: u32 = u32::MAX;

/// General placement entry point (DESIGN.md §10): walks `slots` in
/// order, excluding every disk already chosen for an earlier slot of
/// this file so replicas never collide, and writes each slot's chosen
/// candidate index (or `UNPLACED`) to `out`. `slots.len()` is capped at
/// `MAX_SELECT`, same as `hrw_select`.
///
/// For the common case — every slot `AnyOfClass(None)` — this selects
/// exactly what `hrw_select` would; §10's "one mechanism" design goal is
/// that simple replication is not a special case of this function, it's
/// the same exclusion loop with every constraint left empty.
pub fn place_redundancy(
    candidates: &[DiskCandidate],
    key: u64,
    slots: &[ReplicaSlotSpec],
    out: &mut [u32],
) -> usize {
    let n = core::cmp::min(slots.len(), core::cmp::min(out.len(), MAX_SELECT));
    let mut excluded = [0u32; MAX_SELECT];
    let mut filled_excl = 0usize;

    for (i, slot) in slots.iter().enumerate().take(n) {
        let chosen = if slot.pinned != 0 {
            candidates.iter().enumerate().find_map(|(idx, c)| {
                let already_used = excluded[..filled_excl].contains(&(idx as u32));
                if c.active != 0
                    && !already_used
                    && c.disk_id_hi == slot.pinned_id_hi
                    && c.disk_id_lo == slot.pinned_id_lo
                {
                    Some(idx as u32)
                } else {
                    None
                }
            })
        } else {
            hrw_select_one(
                candidates,
                key,
                &excluded[..filled_excl],
                slot.required_class,
            )
        };

        match chosen {
            Some(idx) => {
                out[i] = idx;
                if filled_excl < MAX_SELECT {
                    excluded[filled_excl] = idx;
                    filled_excl += 1;
                }
            }
            None => out[i] = UNPLACED,
        }
    }
    n
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

/// C ABI entry point for `place_redundancy`.
///
/// # Safety
/// `candidates` must point to `n_candidates` valid `DiskCandidate`s,
/// `slots` to `n_slots` valid `ReplicaSlotSpec`s, and `out` to `n_out`
/// valid, writable `u32`s, for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn tartine_place_redundancy(
    candidates: *const DiskCandidate,
    n_candidates: usize,
    key: u64,
    slots: *const ReplicaSlotSpec,
    n_slots: usize,
    out: *mut u32,
    n_out: usize,
) -> usize {
    if candidates.is_null() || slots.is_null() || out.is_null() {
        return 0;
    }
    let candidates = core::slice::from_raw_parts(candidates, n_candidates);
    let slots = core::slice::from_raw_parts(slots, n_slots);
    let out = core::slice::from_raw_parts_mut(out, n_out);
    place_redundancy(candidates, key, slots, out)
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

    fn candidate(id: u64, class: u8) -> DiskCandidate {
        DiskCandidate {
            disk_id_hi: 0,
            disk_id_lo: id,
            weight_milli: 1000,
            active: 1,
            class,
        }
    }

    fn candidates(n: usize) -> Vec<DiskCandidate> {
        (0..n).map(|i| candidate(i as u64 + 1, CLASS_HDD)).collect()
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
        after.push(candidate(999, CLASS_HDD));

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
                class: CLASS_HDD,
            },
            DiskCandidate {
                disk_id_hi: 0,
                disk_id_lo: 2,
                weight_milli: 3000,
                active: 1,
                class: CLASS_HDD,
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

    fn any_slot() -> ReplicaSlotSpec {
        ReplicaSlotSpec {
            required_class: CLASS_ANY,
            pinned: 0,
            _pad: [0; 2],
            pinned_id_hi: 0,
            pinned_id_lo: 0,
        }
    }

    fn class_slot(class: u8) -> ReplicaSlotSpec {
        ReplicaSlotSpec {
            required_class: class,
            pinned: 0,
            _pad: [0; 2],
            pinned_id_hi: 0,
            pinned_id_lo: 0,
        }
    }

    fn pinned_slot(id: u64) -> ReplicaSlotSpec {
        ReplicaSlotSpec {
            required_class: CLASS_ANY,
            pinned: 1,
            _pad: [0; 2],
            pinned_id_hi: 0,
            pinned_id_lo: id,
        }
    }

    #[test]
    fn place_redundancy_matches_hrw_select_for_uniform_any_slots() {
        let c = candidates(6);
        let slots = [any_slot(), any_slot(), any_slot()];
        let mut placed = [0u32; 3];
        let mut expected = [0u32; 3];
        assert_eq!(place_redundancy(&c, 123, &slots, &mut placed), 3);
        assert_eq!(hrw_select(&c, 123, &mut expected), 3);
        assert_eq!(placed, expected);
    }

    #[test]
    fn place_redundancy_honors_per_slot_class() {
        // "one HDD and one SSD" — the example from the design discussion.
        let mut c = candidates(4); // ids 1..=4, all HDD
        c.push(candidate(5, CLASS_SSD));
        c.push(candidate(6, CLASS_SSD));

        let slots = [class_slot(CLASS_HDD), class_slot(CLASS_SSD)];
        let mut out = [0u32; 2];
        assert_eq!(place_redundancy(&c, 7, &slots, &mut out), 2);
        assert!(c[out[0] as usize].class == CLASS_HDD);
        assert!(c[out[1] as usize].class == CLASS_SSD);
    }

    #[test]
    fn place_redundancy_pinned_slot_picks_exact_disk() {
        let c = candidates(5); // ids 1..=5
        let slots = [pinned_slot(3)];
        let mut out = [0u32; 1];
        assert_eq!(place_redundancy(&c, 99, &slots, &mut out), 1);
        assert_eq!(c[out[0] as usize].disk_id_lo, 3);
    }

    #[test]
    fn place_redundancy_pinned_slot_inactive_disk_is_unplaced() {
        let mut c = candidates(5);
        c[2].active = 0; // id 3
        let slots = [pinned_slot(3)];
        let mut out = [0u32; 1];
        assert_eq!(place_redundancy(&c, 99, &slots, &mut out), 1);
        assert_eq!(out[0], UNPLACED);
    }

    #[test]
    fn place_redundancy_unsatisfiable_class_is_unplaced_without_blocking_other_slots() {
        let c = candidates(3); // all HDD, no SSD in the pool
        let slots = [class_slot(CLASS_HDD), class_slot(CLASS_SSD)];
        let mut out = [0u32; 2];
        assert_eq!(place_redundancy(&c, 5, &slots, &mut out), 2);
        assert_ne!(out[0], UNPLACED);
        assert_eq!(out[1], UNPLACED);
    }

    #[test]
    fn place_redundancy_never_double_places_the_same_disk() {
        let c = candidates(3);
        let slots = [any_slot(), any_slot(), any_slot()];
        let mut out = [0u32; 3];
        assert_eq!(place_redundancy(&c, 11, &slots, &mut out), 3);
        let mut sorted = out.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3);
    }
}
