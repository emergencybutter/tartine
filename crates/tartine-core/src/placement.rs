//! Rendezvous (highest random weight, "HRW") placement.
//!
//! Given a key (e.g. `(inode, chunk_seq)`) and the current pool map, picks
//! the `n` disks that should hold a replica. This is a real, working
//! implementation — unlike most of this workspace it needs no external
//! crate and is small enough to unit test directly. See DESIGN.md §7.2
//! for why HRW rather than a CRUSH-style map.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use tartine_proto::{DiskEntry, DiskId, DiskState, PoolMap};

/// Anything hashable can be a placement key. In practice this is
/// `(InodeId, u64)` for a chunk/extent index, but keeping it generic lets
/// the metadata-group bootstrap pointer (DESIGN.md §6) reuse the same
/// scoring function with a fixed well-known key.
pub trait PlacementKey: Hash {}
impl<T: Hash> PlacementKey for T {}

fn score(disk: DiskId, weight: f64, key: &impl PlacementKey) -> u64 {
    let mut hasher = DefaultHasher::new();
    disk.0.hash(&mut hasher);
    key.hash(&mut hasher);
    let raw = hasher.finish();
    // Scale the hash by weight so heavier (larger-capacity) disks win
    // ties more often, without needing a separate weighted-random-sample
    // structure. `raw` is uniform in [0, u64::MAX]; multiplying by a
    // weight in (0, ~a few] and truncating keeps ordering meaningful.
    ((raw as f64) * weight.max(0.000_1)) as u64
}

/// Returns up to `n` active disks for `key`, highest score first. Fewer
/// than `n` come back if the pool doesn't have `n` active disks with the
/// requested role — callers treat that as "under-replicated, repair when
/// possible" rather than an error (DESIGN.md §7.4).
pub fn targets(
    pool: &PoolMap,
    key: &impl PlacementKey,
    n: usize,
    require_role: impl Fn(&DiskEntry) -> bool,
) -> Vec<DiskId> {
    let mut scored: Vec<(u64, DiskId)> = pool
        .disks
        .iter()
        .filter(|(_, entry)| entry.state == DiskState::Active && require_role(entry))
        .map(|(id, entry)| (score(*id, entry.weight, key), *id))
        .collect();

    scored.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    scored.truncate(n);
    scored.into_iter().map(|(_, id)| id).collect()
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
        let t = targets(&pool, &(42u64, 0u64), 3, |e| e.roles.data);
        assert_eq!(t.len(), 3);
        // no duplicates
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

        let keys: Vec<u64> = (0..1000).collect();
        let mut changed = 0;
        for k in &keys {
            let a = targets(&before, k, 2, |e| e.roles.data);
            let b = targets(&after, k, 2, |e| e.roles.data);
            if a != b {
                changed += 1;
            }
        }
        // HRW's defining property: adding one disk to a pool of 10 should
        // only touch a minority of keys (roughly 1/11), nowhere near all
        // of them the way a naive `hash(key) % disk_count` scheme would.
        assert!(changed < keys.len() / 3, "changed = {changed}");
    }
}
