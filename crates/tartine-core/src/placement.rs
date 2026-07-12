//! `Vec`/`PoolMap`-friendly wrapper around `tartine-kcore`'s allocation-free
//! HRW selection, for the FUSE prototype and its tests. This crate does
//! **not** reimplement the placement algorithm — it just converts a
//! `PoolMap` into the fixed-layout `DiskCandidate` array `tartine-kcore`
//! expects, calls the exact function the kernel module will call, and
//! maps the resulting indices back to `DiskId`s. See DESIGN.md's
//! "kernel/" section and `tartine-kcore`'s crate doc comment for why
//! that matters: it means this prototype validates the kernel module's
//! actual placement decisions, not a lookalike.

use tartine_kcore::placement::{hrw_select, DiskCandidate};
use tartine_proto::{DiskEntry, DiskId, DiskState, PoolMap};

/// Anything hashable-as-a-u64 can be a placement key once reduced via
/// `tartine_kcore`'s hash. In practice this is `(InodeId, u64)` for a
/// chunk/extent index; the FFI boundary only deals in raw `u64`s
/// (`tartine_hash_key`), so this wrapper takes the already-reduced key
/// directly rather than a generic `Hash` key as an earlier version of
/// this module did — keeping exactly one hashing implementation in the
/// whole workspace (`tartine-kcore::hash`).
pub fn placement_key(inode: u64, seq: u64) -> u64 {
    tartine_kcore::placement::tartine_hash_key(inode, seq)
}

fn to_candidate(id: DiskId, entry: &DiskEntry) -> DiskCandidate {
    let bytes = id.0.to_be_bytes();
    let hi = u64::from_be_bytes(bytes[0..8].try_into().unwrap());
    let lo = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
    DiskCandidate {
        disk_id_hi: hi,
        disk_id_lo: lo,
        weight_milli: (entry.weight.max(0.0) * 1000.0) as u32,
        active: (entry.state == DiskState::Active) as u8,
    }
}

/// Returns up to `n` active disks for `key`, highest score first.
pub fn targets(
    pool: &PoolMap,
    key: u64,
    n: usize,
    require_role: impl Fn(&DiskEntry) -> bool,
) -> Vec<DiskId> {
    let ids: Vec<DiskId> = pool
        .disks
        .iter()
        .filter(|(_, entry)| require_role(entry))
        .map(|(id, _)| *id)
        .collect();
    let candidates: Vec<DiskCandidate> = ids
        .iter()
        .map(|id| to_candidate(*id, &pool.disks[id]))
        .collect();

    let mut out = vec![0u32; n.min(tartine_kcore::placement::MAX_SELECT)];
    let filled = hrw_select(&candidates, key, &mut out);
    out[..filled].iter().map(|&idx| ids[idx as usize]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tartine_proto::{DiskRoles, Uuid};

    fn pool_of(n: usize) -> PoolMap {
        let mut disks = HashMap::new();
        for i in 0..n {
            disks.insert(
                Uuid(i as u128 + 1),
                DiskEntry {
                    roles: DiskRoles {
                        data: true,
                        metadata: false,
                    },
                    state: DiskState::Active,
                    weight: 1.0,
                    used_bytes: 0,
                    capacity_bytes: 1,
                },
            );
        }
        PoolMap { epoch: 0, disks }
    }

    #[test]
    fn picks_requested_count() {
        let pool = pool_of(5);
        let key = placement_key(42, 0);
        let t = targets(&pool, key, 3, |e| e.roles.data);
        assert_eq!(t.len(), 3);
        assert_eq!(t.iter().collect::<std::collections::HashSet<_>>().len(), 3);
    }

    #[test]
    fn adding_a_disk_moves_a_minority_of_keys() {
        let before = pool_of(10);
        let mut after = before.clone();
        after.disks.insert(
            Uuid(999),
            DiskEntry {
                roles: DiskRoles {
                    data: true,
                    metadata: false,
                },
                state: DiskState::Active,
                weight: 1.0,
                used_bytes: 0,
                capacity_bytes: 1,
            },
        );

        let mut changed = 0;
        for k in 0u64..1000 {
            let key = placement_key(k, 0);
            let a = targets(&before, key, 2, |e| e.roles.data);
            let b = targets(&after, key, 2, |e| e.roles.data);
            if a != b {
                changed += 1;
            }
        }
        assert!(changed < 1000 / 3, "changed = {changed}");
    }
}
