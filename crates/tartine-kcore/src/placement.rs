//! Rendezvous (HRW) placement — the kernel-safe implementation. This is
//! the *only* implementation of the algorithm in the whole workspace:
//! `tartine-core::placement` (used by the FUSE prototype, DESIGN.md's
//! "prototype first" path) is a thin `Vec`-based wrapper that calls
//! straight into `hrw_select` below, specifically so the prototype and
//! the eventual kernel module are provably running the same placement
//! decisions, not two implementations that could drift apart.
//!
//! No allocation, no floating point (kernel code doesn't get an FPU for
//! free — see DESIGN.md's kernel-build notes), fixed upper bound on `n`
//! so the working set fits on the stack.

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
    pub weight_milli: u32,
    /// 0/1 rather than `bool` — `bool`'s layout across an FFI boundary
    /// with a C caller is easiest to reason about as an explicit `u8`.
    pub active: u8,
}

/// Hard cap on replicas selectable in one call, so `hrw_select` can use a
/// fixed-size stack scratch buffer instead of allocating. Any realistic
/// replication factor (data or metadata) is far below this.
pub const MAX_SELECT: usize = 16;

pub fn hrw_score(disk_id_hi: u64, disk_id_lo: u64, key: u64, weight_milli: u32) -> u64 {
    let id = ((disk_id_hi as u128) << 64) | disk_id_lo as u128;
    let raw = crate::hash::hash_u128_u64(id, key);
    let weight = core::cmp::max(weight_milli, 1) as u64;
    // Shift down first so the weight multiply can't wrap: raw is already
    // uniform over its bit range, so discarding low bits before scaling
    // preserves that while giving headroom for `weight` (typically a few
    // thousand at most) without overflowing u64.
    (raw >> 16).wrapping_mul(weight)
}

/// Selects up to `out.len()` (capped at `MAX_SELECT`) active candidates
/// for `key`, highest score first, writing their *index into
/// `candidates`* to `out`. Returns the number written — fewer than
/// requested if there aren't enough active candidates, which callers
/// treat as "under-replicated, repair when possible" rather than an
/// error (DESIGN.md §7.4).
pub fn hrw_select(candidates: &[DiskCandidate], key: u64, out: &mut [u32]) -> usize {
    let want = core::cmp::min(out.len(), MAX_SELECT);
    let mut top: [(u64, u32); MAX_SELECT] = [(0, 0); MAX_SELECT];
    let mut filled = 0usize;

    for (idx, c) in candidates.iter().enumerate() {
        if c.active == 0 {
            continue;
        }
        let score = hrw_score(c.disk_id_hi, c.disk_id_lo, key, c.weight_milli);

        if filled < want {
            let mut pos = filled;
            while pos > 0 && top[pos - 1].0 < score {
                top[pos] = top[pos - 1];
                pos -= 1;
            }
            top[pos] = (score, idx as u32);
            filled += 1;
        } else if want > 0 && score > top[want - 1].0 {
            let mut pos = want - 1;
            while pos > 0 && top[pos - 1].0 < score {
                top[pos] = top[pos - 1];
                pos -= 1;
            }
            top[pos] = (score, idx as u32);
        }
    }

    for (i, slot) in out.iter_mut().enumerate().take(filled) {
        *slot = top[i].1;
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
}
