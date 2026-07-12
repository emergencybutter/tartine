//! `Vec`/`PoolMap`-friendly wrapper around `tartine-kcore`'s allocation-free
//! HRW selection, for the FUSE prototype and its tests. This crate does
//! **not** reimplement the placement algorithm — it just converts a
//! `PoolMap` (and, for `place_redundancy`, a file's `RedundancyScheme`)
//! into the fixed-layout types `tartine-kcore` expects, calls the exact
//! functions the kernel module will call, and maps the results back to
//! `DiskId`s. See DESIGN.md's "kernel/" section and `tartine-kcore`'s
//! crate doc comment for why that matters: it means this prototype
//! validates the kernel module's actual placement decisions, not a
//! lookalike.

use tartine_kcore::placement::{
    hrw_select, place_redundancy as kcore_place_redundancy, DiskCandidate, ReplicaSlotSpec,
    UNPLACED,
};
use tartine_proto::{DiskEntry, DiskId, DiskState, PoolMap, RedundancyScheme, ReplicaSlot};

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

fn disk_id_to_hi_lo(id: DiskId) -> (u64, u64) {
    let bytes = id.0.to_be_bytes();
    let hi = u64::from_be_bytes(bytes[0..8].try_into().unwrap());
    let lo = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
    (hi, lo)
}

fn to_candidate(id: DiskId, entry: &DiskEntry) -> DiskCandidate {
    let (hi, lo) = disk_id_to_hi_lo(id);
    DiskCandidate {
        disk_id_hi: hi,
        disk_id_lo: lo,
        weight_milli: (entry.weight.max(0.0) * 1000.0) as u32,
        active: (entry.state == DiskState::Active) as u8,
        class: entry.class.to_ffi(),
    }
}

/// Returns up to `n` active disks for `key`, highest score first. The
/// plain "N any disks" case — for per-slot class constraints or pinning,
/// see `place_redundancy` below.
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

#[derive(Debug, PartialEq, Eq)]
pub enum PlaceError {
    /// DESIGN.md §16.6: reserved syntax, no placement/read/repair path
    /// implemented yet.
    ErasureCodedUnimplemented,
}

fn slot_to_spec(slot: &ReplicaSlot) -> ReplicaSlotSpec {
    match slot {
        ReplicaSlot::AnyOfClass(None) => ReplicaSlotSpec {
            required_class: tartine_kcore::placement::CLASS_ANY,
            pinned: 0,
            _pad: [0; 2],
            pinned_id_hi: 0,
            pinned_id_lo: 0,
        },
        ReplicaSlot::AnyOfClass(Some(class)) => ReplicaSlotSpec {
            required_class: class.to_ffi(),
            pinned: 0,
            _pad: [0; 2],
            pinned_id_hi: 0,
            pinned_id_lo: 0,
        },
        ReplicaSlot::Pinned(id) => {
            let (hi, lo) = disk_id_to_hi_lo(*id);
            ReplicaSlotSpec {
                required_class: tartine_kcore::placement::CLASS_ANY,
                pinned: 1,
                _pad: [0; 2],
                pinned_id_hi: hi,
                pinned_id_lo: lo,
            }
        }
    }
}

/// General placement entry point (DESIGN.md §10): places each replica
/// slot of `scheme`, in order, excluding disks already chosen for an
/// earlier slot of the same file. Each element of the result is `None`
/// if that slot couldn't be satisfied (pinned disk missing/inactive, or
/// no remaining `require_role` disk of the requested class) — DESIGN.md
/// §10 treats that as a hard error at policy-set time and ordinary
/// under-replication (repair retries later) for an already-placed file.
pub fn place_redundancy(
    pool: &PoolMap,
    scheme: &RedundancyScheme,
    key: u64,
    require_role: impl Fn(&DiskEntry) -> bool,
) -> Result<Vec<Option<DiskId>>, PlaceError> {
    let RedundancyScheme::Replicated(slots) = scheme else {
        return Err(PlaceError::ErasureCodedUnimplemented);
    };

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
    let specs: Vec<ReplicaSlotSpec> = slots.iter().map(slot_to_spec).collect();

    let mut out = vec![0u32; specs.len().min(tartine_kcore::placement::MAX_SELECT)];
    let n = kcore_place_redundancy(&candidates, key, &specs, &mut out);
    Ok(out[..n]
        .iter()
        .map(|&idx| {
            if idx == UNPLACED {
                None
            } else {
                Some(ids[idx as usize])
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tartine_proto::{DiskClass, DiskRoles, Uuid};

    fn disk_entry(class: DiskClass) -> DiskEntry {
        DiskEntry {
            roles: DiskRoles {
                data: true,
                metadata: false,
            },
            state: DiskState::Active,
            class,
            weight: 1.0,
            used_bytes: 0,
            capacity_bytes: 1,
        }
    }

    fn pool_of(n: usize) -> PoolMap {
        let mut disks = HashMap::new();
        for i in 0..n {
            disks.insert(Uuid(i as u128 + 1), disk_entry(DiskClass::Hdd));
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
        after.disks.insert(Uuid(999), disk_entry(DiskClass::Hdd));

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

    #[test]
    fn place_redundancy_one_hdd_one_ssd() {
        let mut pool = pool_of(3); // all HDD
        pool.disks.insert(Uuid(100), disk_entry(DiskClass::Ssd));
        pool.disks.insert(Uuid(101), disk_entry(DiskClass::Ssd));

        let scheme = RedundancyScheme::Replicated(vec![
            ReplicaSlot::AnyOfClass(Some(DiskClass::Hdd)),
            ReplicaSlot::AnyOfClass(Some(DiskClass::Ssd)),
        ]);
        let placed = place_redundancy(&pool, &scheme, 7, |e| e.roles.data).unwrap();
        assert_eq!(placed.len(), 2);
        let hdd_disk = placed[0].expect("hdd slot placed");
        let ssd_disk = placed[1].expect("ssd slot placed");
        assert_eq!(pool.disks[&hdd_disk].class, DiskClass::Hdd);
        assert_eq!(pool.disks[&ssd_disk].class, DiskClass::Ssd);
    }

    #[test]
    fn place_redundancy_pinned_disk() {
        let pool = pool_of(5);
        let pinned = Uuid(3);
        let scheme = RedundancyScheme::Replicated(vec![ReplicaSlot::Pinned(pinned)]);
        let placed = place_redundancy(&pool, &scheme, 42, |e| e.roles.data).unwrap();
        assert_eq!(placed, vec![Some(pinned)]);
    }

    #[test]
    fn place_redundancy_unreplicated_any_disk_is_just_one_slot() {
        let pool = pool_of(5);
        let scheme = RedundancyScheme::Replicated(vec![ReplicaSlot::AnyOfClass(None)]);
        let placed = place_redundancy(&pool, &scheme, 1, |e| e.roles.data).unwrap();
        assert_eq!(placed.len(), 1);
        assert!(placed[0].is_some());
    }

    #[test]
    fn place_redundancy_rejects_erasure_coded() {
        let pool = pool_of(5);
        let scheme = RedundancyScheme::ErasureCoded {
            data_shards: 4,
            parity_shards: 2,
        };
        assert_eq!(
            place_redundancy(&pool, &scheme, 1, |e| e.roles.data),
            Err(PlaceError::ErasureCodedUnimplemented)
        );
    }
}
