//! Shared wire / on-disk types for TartineFS.
//!
//! Nothing in this crate does I/O — it is the vocabulary every other
//! crate (`tartine-core`, `tartine-meta`, `tartine-fuse`, `tartined`,
//! `tartinectl`) shares, so that pool map, inode, and placement types
//! only have one definition. See `DESIGN.md` at the repo root for the
//! rationale behind each type.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// 128-bit random identifier. A real implementation would draw this from
/// `getrandom(2)`; kept as a thin newtype here so every id in the system
/// (disks, pools, inodes-are-u64-though) shares one type and one
/// "generate a fresh one" call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Uuid(pub u128);

impl Uuid {
    pub fn nil() -> Self {
        Uuid(0)
    }
}

pub type InodeId = u64;
pub type DiskId = Uuid;
pub type PoolId = Uuid;

/// A disk can serve file data, hold a metadata replica, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskRoles {
    pub data: bool,
    pub metadata: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskState {
    /// Fully in service, eligible as an HRW placement target.
    Active,
    /// Being emptied by the rebalancer ahead of removal; still readable,
    /// no longer a placement target for new writes.
    Draining,
    /// Confirmed gone (failed or force-removed). Anything it held is
    /// under-replicated until the repair loop catches up.
    Dead,
}

#[derive(Debug, Clone)]
pub struct DiskEntry {
    pub roles: DiskRoles,
    pub state: DiskState,
    /// Relative placement weight, default proportional to capacity.
    pub weight: f64,
    pub used_bytes: u64,
    pub capacity_bytes: u64,
}

/// The pool's membership, versioned by `epoch`. Every membership change
/// (add/remove/drain a disk) bumps `epoch`; placement decisions and
/// on-disk "last epoch I observed" bookkeeping (`SuperBlock::last_seen_epoch`)
/// both key off this number. See DESIGN.md §7.1.
#[derive(Debug, Clone, Default)]
pub struct PoolMap {
    pub epoch: u64,
    pub disks: HashMap<DiskId, DiskEntry>,
}

/// First 4 KiB of every pool disk. See DESIGN.md §5.2.
#[derive(Debug, Clone)]
pub struct SuperBlock {
    pub pool_id: PoolId,
    pub disk_id: DiskId,
    pub roles: DiskRoles,
    pub format_version: u32,
    pub created_at_unix: u64,
    pub last_seen_epoch: u64,
}

impl SuperBlock {
    pub fn new(pool_id: PoolId, disk_id: DiskId, roles: DiskRoles) -> Self {
        let created_at_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_secs();
        SuperBlock {
            pool_id,
            disk_id,
            roles,
            format_version: 1,
            created_at_unix,
            last_seen_epoch: 0,
        }
    }
}

/// The two (in v1, exactly two) disks holding the synchronously replicated
/// metadata store, plus the fencing epoch used to detect and reject a
/// stale member after a failover. See DESIGN.md §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaGroup {
    pub primary: DiskId,
    pub backup: Option<DiskId>,
    pub epoch: u64,
}

/// A file's lifecycle. `AppendOnly` is the only state a file is ever
/// *created* in; `Writable` is reached only via `Converting` and is
/// terminal (no path back). See DESIGN.md §9.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeMode {
    AppendOnly,
    Converting,
    Writable,
}

/// One entry in an append-only file's chunk-log pointer list: which
/// sequence number, and which disks (in replica order) hold it.
#[derive(Debug, Clone)]
pub struct ChunkPointer {
    pub chunk_seq: u64,
    pub replicas: Vec<DiskId>,
    pub len: u32,
    pub checksum: u64,
}

/// One entry in a writable file's extent map: a fixed-size, block-aligned
/// range and the disks holding it.
#[derive(Debug, Clone)]
pub struct Extent {
    pub file_offset: u64,
    pub len: u32,
    pub replicas: Vec<DiskId>,
    pub checksum: u64,
}

#[derive(Debug, Clone)]
pub enum DataLocator {
    ChunkLog(Vec<ChunkPointer>),
    Extents(Vec<Extent>),
}

#[derive(Debug, Clone)]
pub struct InodeRecord {
    pub inode: InodeId,
    pub mode: InodeMode,
    pub size: u64,
    /// Desired replica count for this file's data; independent of the
    /// pool-wide metadata replication factor, which is always 2 in v1.
    /// See DESIGN.md §10.
    pub replication_factor: u8,
    pub data: DataLocator,
    pub uid: u32,
    pub gid: u32,
    pub unix_mode: u32,
    pub mtime_unix: u64,
}

/// Metadata mutations are logged as `MetaOp`s before being applied, and
/// shipped verbatim to both metadata disks (DESIGN.md §6) before a write
/// is acknowledged to the caller.
#[derive(Debug, Clone)]
pub enum MetaOp {
    CreateInode(InodeRecord),
    Link {
        parent: InodeId,
        name: String,
        child: InodeId,
    },
    Unlink {
        parent: InodeId,
        name: String,
    },
    AppendChunk {
        inode: InodeId,
        chunk: ChunkPointer,
    },
    BeginConvert {
        inode: InodeId,
    },
    CompleteConvert {
        inode: InodeId,
        extents: Vec<Extent>,
    },
    SetReplicationFactor {
        inode: InodeId,
        factor: u8,
    },
    PoolMapChange(PoolMap),
    MetaGroupChange(MetaGroup),
}
