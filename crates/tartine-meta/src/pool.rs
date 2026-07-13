//! `Pool`: ties disks + placement + the metadata store together into one
//! addressable thing (IMPLEMENTATION_PLAN.md P1.4). Lives here rather
//! than in `tartine-core` because it needs both `tartine-core`
//! (segment/placement) *and* `MetaStore` (this crate); `tartine-meta`
//! already depends on `tartine-core`, so this direction avoids a
//! dependency cycle.
//!
//! **On-disk layout per disk file** (fixed, no real superblock format
//! yet — flagged as a gap, not silently skipped: DESIGN.md §5.2 defines
//! one, `kernel/tartine_main.c` reads it, but this prototype doesn't
//! write it, so `Pool::open` re-derives disk identity from argument
//! order rather than reading it back from disk):
//!
//! ```text
//! [0, 4096)                                     reserved for a superblock (unwritten)
//! [4096, 4096 + WAL_CAPACITY_BYTES)              metadata WAL region
//! [SEGMENT_BASE_OFFSET, + SEGMENT_SIZE_BYTES)    chunk-log / extent data region
//! ```
//!
//! The first two disks passed to `format`/`open` become the metadata
//! group (just the first, if there is no second — DESIGN.md §6's
//! metadata replication factor is `min(2, disk count)`, so a 1-disk
//! pool runs with a single, unreplicated metadata copy rather than
//! being rejected); every disk gets the data role, so on a 1-disk pool
//! that one disk necessarily holds both the metadata WAL and file
//! data. Real superblock-based role/ID discovery is P1.7/kernel-side
//! work (DESIGN.md §7), out of scope here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tartine_core::disk::{Disk, FileDisk};
use tartine_core::placement::{self, PlaceError};
use tartine_core::segment::{self, Record, SegmentWriter};
use tartine_proto::{
    ChunkPointer, DataLocator, DiskClass, DiskEntry, DiskId, DiskRoles, DiskState, Extent, InodeId,
    InodeMode, InodeRecord, MetaGroup, MetaOp, PoolMap, RedundancyScheme, Uuid,
};

use crate::store::{MetaStore, MetaStoreError};
use crate::wal;

pub const SUPERBLOCK_RESERVE_BYTES: u64 = 4096;
pub const WAL_BASE_OFFSET: u64 = SUPERBLOCK_RESERVE_BYTES;
pub const SEGMENT_BASE_OFFSET: u64 = WAL_BASE_OFFSET + wal::WAL_CAPACITY_BYTES;
pub const DISK_FILE_CAPACITY_BYTES: u64 = SEGMENT_BASE_OFFSET + segment::SEGMENT_SIZE_BYTES;

pub const ROOT_INODE: InodeId = 1;

/// `InodeRecord.unix_mode` carries the POSIX file-type bits (`S_IFDIR`/
/// `S_IFREG`) in its upper bits alongside the permission bits, same
/// convention as `st_mode` from `stat(2)` — this is what lets a reader
/// (e.g. `tartine-fuse`'s `getattr`) tell a directory from a regular
/// file without a separate `is_dir` field. `InodeMode` (`AppendOnly`/
/// `Converting`/`Writable`) is an orthogonal axis: directories are
/// always `Writable` (DESIGN.md's kernel skeleton makes the same
/// choice — see `kernel/tartine_main.c`'s `tartine_iget`), regular
/// files start `AppendOnly`.
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;

#[derive(Debug)]
pub enum PoolError {
    Io(std::io::Error),
    Meta(MetaStoreError),
    /// A replica slot couldn't be placed (DESIGN.md §10.2/§10.3): at
    /// policy-set / write time this is a hard error, not silent
    /// under-replication (there's no repair loop yet to fix it up
    /// later — that's P1.8).
    Unplaceable,
    NotFound,
    /// The operation requires the file to be `AppendOnly`/`Converting`
    /// (append) or `Writable` (random write) and it wasn't.
    WrongMode,
    NoDisks,
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolError::Io(e) => write!(f, "I/O error: {e}"),
            PoolError::Meta(e) => write!(f, "metadata error: {e}"),
            PoolError::Unplaceable => write!(f, "could not place all required replicas"),
            PoolError::NotFound => write!(f, "no such inode"),
            PoolError::WrongMode => write!(f, "operation not valid for this file's current mode"),
            PoolError::NoDisks => write!(f, "a pool needs at least 1 disk"),
        }
    }
}
impl std::error::Error for PoolError {}
impl From<std::io::Error> for PoolError {
    fn from(e: std::io::Error) -> Self {
        PoolError::Io(e)
    }
}
impl From<MetaStoreError> for PoolError {
    fn from(e: MetaStoreError) -> Self {
        PoolError::Meta(e)
    }
}
impl From<PlaceError> for PoolError {
    fn from(_: PlaceError) -> Self {
        PoolError::Unplaceable
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs()
}

struct DiskHandle {
    disk: Arc<FileDisk>,
    /// Bump-allocator cursor into this disk's data region
    /// (`SEGMENT_BASE_OFFSET..`), shared by chunk-log appends *and*
    /// extent storage (DESIGN.md §5.3 vs. §11's fixed-size extents both
    /// simplify, for the prototype, to "the next free byte on this
    /// disk's data region" — see `complete_convert`/`write` below).
    cursor: u64,
}

pub struct Pool {
    pool_map: PoolMap,
    disks: HashMap<DiskId, DiskHandle>,
    meta: MetaStore<Arc<FileDisk>>,
}

impl Pool {
    fn disk_ids_for(disk_paths: &[PathBuf]) -> Vec<DiskId> {
        (0..disk_paths.len()).map(|i| Uuid(i as u128 + 1)).collect()
    }

    /// Creates a brand-new pool with every disk defaulted to
    /// `DiskClass::Hdd` — see `format_with_classes` for real control
    /// over per-disk class (DESIGN.md §10.1's manual-override half of
    /// "auto-detected with a manual override"; the auto-detection half
    /// isn't implemented in this prototype).
    pub fn format(disk_paths: &[PathBuf], redb_path: &Path) -> Result<Self, PoolError> {
        let classes = vec![DiskClass::Hdd; disk_paths.len()];
        Self::format_with_classes(disk_paths, &classes, redb_path)
    }

    /// Creates a brand-new pool: formats every disk file (tagged with
    /// the given `classes`, parallel to `disk_paths`), brings up the
    /// metadata group on the first two (or just the first, if there is
    /// no second — see DESIGN.md §6's single-disk note: the metadata
    /// replication factor is `min(2, disk count)`, so a 1-disk pool
    /// starts in the same primary-only state §6 already describes for
    /// backup failure), and creates the root directory inode as the
    /// WAL's first entry.
    pub fn format_with_classes(
        disk_paths: &[PathBuf],
        classes: &[DiskClass],
        redb_path: &Path,
    ) -> Result<Self, PoolError> {
        if disk_paths.is_empty() {
            return Err(PoolError::NoDisks);
        }
        assert_eq!(disk_paths.len(), classes.len(), "one class per disk path");
        let ids = Self::disk_ids_for(disk_paths);

        let mut pool_map = PoolMap {
            epoch: 0,
            disks: HashMap::new(),
        };
        let mut disks = HashMap::new();
        let mut arcs = Vec::with_capacity(disk_paths.len());
        for (i, path) in disk_paths.iter().enumerate() {
            let arc = Arc::new(FileDisk::create(path, DISK_FILE_CAPACITY_BYTES)?);
            pool_map.disks.insert(
                ids[i],
                DiskEntry {
                    roles: DiskRoles {
                        data: true,
                        metadata: i < 2,
                    },
                    state: DiskState::Active,
                    class: classes[i],
                    weight: 1.0,
                    used_bytes: 0,
                    capacity_bytes: DISK_FILE_CAPACITY_BYTES,
                },
            );
            disks.insert(
                ids[i],
                DiskHandle {
                    disk: arc.clone(),
                    cursor: 0,
                },
            );
            arcs.push(arc);
        }

        let group = MetaGroup {
            primary: ids[0],
            backup: ids.get(1).copied(),
            epoch: 0,
        };
        let backup_arc = ids.get(1).map(|_| arcs[1].clone());
        let mut meta = MetaStore::open(group, arcs[0].clone(), backup_arc, redb_path)?;

        let root =
            InodeRecord {
                inode: ROOT_INODE,
                mode: InodeMode::Writable,
                size: 0,
                redundancy: RedundancyScheme::Replicated(vec![
                tartine_proto::ReplicaSlot::AnyOfClass(None);
                2.min(disk_paths.len())
            ]),
                data: DataLocator::Extents(vec![]),
                uid: 0,
                gid: 0,
                unix_mode: S_IFDIR | 0o755,
                mtime_unix: now_unix(),
            };
        meta.apply(MetaOp::CreateInode(root))?;

        Ok(Pool {
            pool_map,
            disks,
            meta,
        })
    }

    /// Reopens an existing pool with every disk defaulted to
    /// `DiskClass::Hdd` — see `open_with_classes`. Since there's no real
    /// superblock yet (this prototype's documented gap — see the module
    /// doc comment), class assignments aren't persisted either, so a
    /// pool formatted with real classes must be *reopened* with the same
    /// classes given again for placement decisions to stay correct.
    pub fn open(disk_paths: &[PathBuf], redb_path: &Path) -> Result<Self, PoolError> {
        let classes = vec![DiskClass::Hdd; disk_paths.len()];
        Self::open_with_classes(disk_paths, &classes, redb_path)
    }

    /// Reopens an existing pool: replays each metadata disk's WAL
    /// (`MetaStore::open`) and recovery-scans each disk's segment/data
    /// region (`segment::scan`) to find where each disk's write cursor
    /// should resume — this is the crash-recovery path, exercising
    /// P1.2's and P1.3's recovery logic together for the first time.
    pub fn open_with_classes(
        disk_paths: &[PathBuf],
        classes: &[DiskClass],
        redb_path: &Path,
    ) -> Result<Self, PoolError> {
        if disk_paths.is_empty() {
            return Err(PoolError::NoDisks);
        }
        assert_eq!(disk_paths.len(), classes.len(), "one class per disk path");
        let ids = Self::disk_ids_for(disk_paths);

        let mut pool_map = PoolMap {
            epoch: 0,
            disks: HashMap::new(),
        };
        let mut disks = HashMap::new();
        let mut arcs = Vec::with_capacity(disk_paths.len());
        for (i, path) in disk_paths.iter().enumerate() {
            let arc = Arc::new(FileDisk::open(path)?);
            let scan = segment::scan(&arc, SEGMENT_BASE_OFFSET)?;
            pool_map.disks.insert(
                ids[i],
                DiskEntry {
                    roles: DiskRoles {
                        data: true,
                        metadata: i < 2,
                    },
                    state: DiskState::Active,
                    class: classes[i],
                    weight: 1.0,
                    used_bytes: 0,
                    capacity_bytes: DISK_FILE_CAPACITY_BYTES,
                },
            );
            disks.insert(
                ids[i],
                DiskHandle {
                    disk: arc.clone(),
                    cursor: scan.resume_at,
                },
            );
            arcs.push(arc);
        }

        let group = MetaGroup {
            primary: ids[0],
            backup: ids.get(1).copied(),
            epoch: 0,
        };
        let backup_arc = ids.get(1).map(|_| arcs[1].clone());
        let meta = MetaStore::open(group, arcs[0].clone(), backup_arc, redb_path)?;

        Ok(Pool {
            pool_map,
            disks,
            meta,
        })
    }

    /// Number of disks in the pool — used to scale default per-file
    /// redundancy the same way the root inode's is scaled (see
    /// `format_with_classes`): a small pool can't satisfy a 2-replica
    /// scheme, so callers should ask for `2.min(disk_count())` instead
    /// of a hardcoded 2.
    pub fn disk_count(&self) -> usize {
        self.disks.len()
    }

    pub fn alloc_inode(&mut self) -> InodeId {
        self.meta.alloc_inode()
    }
    pub fn get_inode(&self, id: InodeId) -> Result<Option<InodeRecord>, PoolError> {
        Ok(self.meta.get_inode(id)?)
    }
    pub fn lookup(&self, parent: InodeId, name: &str) -> Result<Option<InodeId>, PoolError> {
        Ok(self.meta.lookup(parent, name)?)
    }
    pub fn readdir(&self, parent: InodeId) -> Result<Vec<(String, InodeId)>, PoolError> {
        Ok(self.meta.readdir(parent)?)
    }

    /// Creates a new append-only file and links it into `parent`.
    pub fn create_file(
        &mut self,
        parent: InodeId,
        name: &str,
        redundancy: RedundancyScheme,
        uid: u32,
        gid: u32,
        unix_mode: u32,
    ) -> Result<InodeId, PoolError> {
        let inode = self.meta.alloc_inode();
        let rec = InodeRecord {
            inode,
            mode: InodeMode::AppendOnly,
            size: 0,
            redundancy,
            data: DataLocator::ChunkLog(vec![]),
            uid,
            gid,
            unix_mode,
            mtime_unix: now_unix(),
        };
        self.meta.apply(MetaOp::CreateInode(rec))?;
        self.meta.apply(MetaOp::Link {
            parent,
            name: name.to_string(),
            child: inode,
        })?;
        Ok(inode)
    }

    pub fn unlink(&mut self, parent: InodeId, name: &str) -> Result<(), PoolError> {
        self.meta.apply(MetaOp::Unlink {
            parent,
            name: name.to_string(),
        })?;
        Ok(())
    }

    pub fn set_redundancy(
        &mut self,
        inode: InodeId,
        scheme: RedundancyScheme,
    ) -> Result<(), PoolError> {
        self.meta
            .apply(MetaOp::SetRedundancyScheme { inode, scheme })?;
        Ok(())
    }

    fn place(&self, scheme: &RedundancyScheme, key: u64) -> Result<Vec<DiskId>, PoolError> {
        let placed = placement::place_redundancy(&self.pool_map, scheme, key, |e| e.roles.data)?;
        placed
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or(PoolError::Unplaceable)
    }

    /// Appends `payload` to an `AppendOnly` (or `Converting` — DESIGN.md
    /// §9.3 step 4's tail-append case) file: places replicas per the
    /// file's redundancy policy, writes the record to each, commits
    /// `AppendChunk`.
    pub fn append(&mut self, inode: InodeId, payload: Vec<u8>) -> Result<(), PoolError> {
        let rec = self.meta.get_inode(inode)?.ok_or(PoolError::NotFound)?;
        if !matches!(rec.mode, InodeMode::AppendOnly | InodeMode::Converting) {
            return Err(PoolError::WrongMode);
        }
        let chunk_seq = match &rec.data {
            DataLocator::ChunkLog(chunks) => chunks.len() as u64,
            DataLocator::Extents(_) => return Err(PoolError::WrongMode),
        };

        let key = placement::placement_key(inode, chunk_seq);
        let disk_ids = self.place(&rec.redundancy, key)?;

        let record = Record::new(inode, chunk_seq, payload.clone());
        let mut replicas = Vec::with_capacity(disk_ids.len());
        for disk_id in &disk_ids {
            let handle = self.disks.get_mut(disk_id).ok_or(PoolError::Unplaceable)?;
            let mut writer =
                SegmentWriter::resume(handle.disk.clone(), SEGMENT_BASE_OFFSET, handle.cursor);
            writer.append(&record)?;
            writer.sync()?;
            let written_at = handle.cursor; // offset relative to SEGMENT_BASE_OFFSET, before this append
            handle.cursor = writer.cursor();
            replicas.push((*disk_id, written_at));
        }

        self.meta.apply(MetaOp::AppendChunk {
            inode,
            chunk: ChunkPointer {
                chunk_seq,
                replicas,
                len: payload.len() as u32,
                checksum: record.checksum,
            },
        })?;
        Ok(())
    }

    /// Reads `len` bytes starting at `offset` from `inode`'s current
    /// content, whichever mode it's in. Simplest correct thing for the
    /// prototype: reconstruct the *whole* file's content, then slice —
    /// no partial-chunk-range optimization (DESIGN.md §16.3's
    /// page-cache-shaped read path is future work, not this).
    pub fn read(&self, inode: InodeId, offset: u64, len: usize) -> Result<Vec<u8>, PoolError> {
        let rec = self.meta.get_inode(inode)?.ok_or(PoolError::NotFound)?;
        let content = self.read_full_content(&rec)?;
        let start = (offset as usize).min(content.len());
        let end = start.saturating_add(len).min(content.len());
        Ok(content[start..end].to_vec())
    }

    fn read_full_content(&self, rec: &InodeRecord) -> Result<Vec<u8>, PoolError> {
        match &rec.data {
            DataLocator::ChunkLog(chunks) => {
                let mut out = Vec::with_capacity(rec.size as usize);
                for chunk in chunks {
                    // Chunk records are framed (segment::Record's
                    // magic+inode+chunk_seq+checksum+len header) —
                    // stored offset points at the frame start, so the
                    // payload begins HEADER_LEN bytes further in.
                    out.extend_from_slice(&self.read_replicated(
                        &chunk.replicas,
                        segment::HEADER_LEN as u64,
                        chunk.len as usize,
                    )?);
                }
                Ok(out)
            }
            DataLocator::Extents(extents) => {
                let mut out = vec![0u8; rec.size as usize];
                for extent in extents {
                    // Extents are stored as raw bytes with no framing
                    // (see `write_whole_file_extent`) — no header to skip.
                    let bytes = self.read_replicated(&extent.replicas, 0, extent.len as usize)?;
                    let start = extent.file_offset as usize;
                    out[start..start + bytes.len()].copy_from_slice(&bytes);
                }
                Ok(out)
            }
        }
    }

    /// Reads `len` bytes starting `header_len` bytes past the stored
    /// offset of the first replica in `replicas`. MVP: doesn't retry a
    /// different replica on failure/checksum mismatch — DESIGN.md §8's
    /// scrub/repair-on-read-failure loop is P1.8, not this milestone.
    fn read_replicated(
        &self,
        replicas: &[(DiskId, u64)],
        header_len: u64,
        len: usize,
    ) -> Result<Vec<u8>, PoolError> {
        let (disk_id, offset) = replicas.first().ok_or(PoolError::Unplaceable)?;
        let handle = self.disks.get(disk_id).ok_or(PoolError::NotFound)?;
        let mut buf = vec![0u8; len];
        handle
            .disk
            .read_at(SEGMENT_BASE_OFFSET + offset + header_len, &mut buf)?;
        Ok(buf)
    }

    /// DESIGN.md §9.3 step 1: `AppendOnly -> Converting`.
    pub fn begin_convert(&mut self, inode: InodeId) -> Result<(), PoolError> {
        self.meta.apply(MetaOp::BeginConvert { inode })?;
        Ok(())
    }

    /// DESIGN.md §9.3 steps 2-5, materialized synchronously (the
    /// prototype's `--wait` path — DESIGN.md §9.3's bounded delta-pass
    /// concurrency refinement for the async path is not implemented
    /// here, see IMPLEMENTATION_PLAN.md P1.9): reads the file's full
    /// chunk-log content, re-places it as one contiguous extent per
    /// replica disk (not DESIGN.md §11's fixed-size sub-block extent
    /// map — flagged as prototype scope, matching DESIGN.md §14's own
    /// note that the real extent allocator is future work), commits
    /// `CompleteConvert`.
    pub fn complete_convert(&mut self, inode: InodeId) -> Result<(), PoolError> {
        let rec = self.meta.get_inode(inode)?.ok_or(PoolError::NotFound)?;
        if rec.mode != InodeMode::Converting {
            return Err(PoolError::WrongMode);
        }
        let content = self.read_full_content(&rec)?;
        let extents = self.write_whole_file_extent(inode, &rec.redundancy, 0, &content)?;
        self.meta
            .apply(MetaOp::CompleteConvert { inode, extents })?;
        Ok(())
    }

    /// Random write to an already-`Writable` file (DESIGN.md §11):
    /// reconstructs the current full content, patches in `data` at
    /// `offset` (zero-extending if the write grows the file), re-places
    /// and rewrites the whole thing as one extent, commits
    /// `UpdateExtents`. Correct, not representative of the real
    /// sub-block-extent read-modify-write DESIGN.md §11/§14 describe —
    /// same documented scope limit as `complete_convert`.
    pub fn write(&mut self, inode: InodeId, offset: u64, data: &[u8]) -> Result<(), PoolError> {
        let rec = self.meta.get_inode(inode)?.ok_or(PoolError::NotFound)?;
        if rec.mode != InodeMode::Writable {
            return Err(PoolError::WrongMode);
        }
        let mut content = self.read_full_content(&rec)?;
        let end = offset as usize + data.len();
        if content.len() < end {
            content.resize(end, 0);
        }
        content[offset as usize..end].copy_from_slice(data);

        let extents = self.write_whole_file_extent(inode, &rec.redundancy, 0, &content)?;
        self.meta.apply(MetaOp::UpdateExtents { inode, extents })?;
        Ok(())
    }

    fn write_whole_file_extent(
        &mut self,
        inode: InodeId,
        redundancy: &RedundancyScheme,
        file_offset: u64,
        content: &[u8],
    ) -> Result<Vec<Extent>, PoolError> {
        if content.is_empty() {
            return Ok(vec![]);
        }
        let key = placement::placement_key(inode, u64::MAX); // distinct key space from chunk_seq
        let disk_ids = self.place(redundancy, key)?;
        let checksum = tartine_core::crc32c::checksum(content);

        let mut replicas = Vec::with_capacity(disk_ids.len());
        for disk_id in &disk_ids {
            let handle = self.disks.get_mut(disk_id).ok_or(PoolError::Unplaceable)?;
            let written_at = handle.cursor;
            handle
                .disk
                .write_at(SEGMENT_BASE_OFFSET + written_at, content)?;
            handle.disk.sync()?;
            handle.cursor += content.len() as u64;
            replicas.push((*disk_id, written_at));
        }

        Ok(vec![Extent {
            file_offset,
            len: content.len() as u32,
            replicas,
            checksum,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tartine_proto::ReplicaSlot;

    struct TestPaths {
        disks: Vec<PathBuf>,
        redb: PathBuf,
    }
    impl TestPaths {
        fn new(name: &str, n_disks: usize) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!("tartine-pool-test-{}-{name}", std::process::id()));
            let _ = std::fs::create_dir_all(&p);
            let disks: Vec<PathBuf> = (0..n_disks).map(|i| p.join(format!("disk{i}"))).collect();
            let redb = p.join("meta.redb");
            for f in disks.iter().chain(std::iter::once(&redb)) {
                let _ = std::fs::remove_file(f);
            }
            TestPaths { disks, redb }
        }
    }
    impl Drop for TestPaths {
        fn drop(&mut self) {
            for f in self.disks.iter().chain(std::iter::once(&self.redb)) {
                let _ = std::fs::remove_file(f);
            }
        }
    }

    fn any_n(n: usize) -> RedundancyScheme {
        RedundancyScheme::Replicated(vec![ReplicaSlot::AnyOfClass(None); n])
    }

    #[test]
    fn format_creates_root_and_pool_is_immediately_usable() {
        let t = TestPaths::new("format", 3);
        let pool = Pool::format(&t.disks, &t.redb).unwrap();
        let root = pool.get_inode(ROOT_INODE).unwrap().unwrap();
        assert_eq!(root.mode, InodeMode::Writable);
        assert_eq!(pool.readdir(ROOT_INODE).unwrap(), vec![]);
    }

    #[test]
    fn create_append_and_read_back() {
        let t = TestPaths::new("append", 3);
        let mut pool = Pool::format(&t.disks, &t.redb).unwrap();

        let inode = pool
            .create_file(ROOT_INODE, "f", any_n(2), 1000, 1000, 0o644)
            .unwrap();
        pool.append(inode, b"hello ".to_vec()).unwrap();
        pool.append(inode, b"world".to_vec()).unwrap();

        assert_eq!(pool.lookup(ROOT_INODE, "f").unwrap(), Some(inode));
        assert_eq!(pool.read(inode, 0, 100).unwrap(), b"hello world");
        assert_eq!(pool.read(inode, 6, 5).unwrap(), b"world");
        assert_eq!(pool.get_inode(inode).unwrap().unwrap().size, 11);
    }

    #[test]
    fn append_rejects_wrong_mode() {
        let t = TestPaths::new("wrongmode", 2);
        let mut pool = Pool::format(&t.disks, &t.redb).unwrap();
        // The root inode is created Writable (it's a directory), so
        // appending to it must fail exactly like a truncate-write would
        // against a real AppendOnly file in the wrong direction.
        assert!(matches!(
            pool.append(ROOT_INODE, b"x".to_vec()),
            Err(PoolError::WrongMode)
        ));
    }

    #[test]
    fn convert_then_random_write_round_trips() {
        let t = TestPaths::new("convert", 2);
        let mut pool = Pool::format(&t.disks, &t.redb).unwrap();

        let inode = pool
            .create_file(ROOT_INODE, "f", any_n(2), 0, 0, 0o644)
            .unwrap();
        pool.append(inode, b"hello\n".to_vec()).unwrap();
        assert_eq!(pool.read(inode, 0, 100).unwrap(), b"hello\n");

        pool.begin_convert(inode).unwrap();
        assert_eq!(
            pool.get_inode(inode).unwrap().unwrap().mode,
            InodeMode::Converting
        );
        pool.complete_convert(inode).unwrap();
        let rec = pool.get_inode(inode).unwrap().unwrap();
        assert_eq!(rec.mode, InodeMode::Writable);
        assert!(matches!(rec.data, DataLocator::Extents(_)));
        // Materialization must not have changed the content.
        assert_eq!(pool.read(inode, 0, 100).unwrap(), b"hello\n");

        // In-place write, like `dd bs=1 seek=2 count=1 conv=notrunc`:
        // overwrite the 3rd byte ('l') with 'L'.
        pool.write(inode, 2, b"L").unwrap();
        assert_eq!(pool.read(inode, 0, 100).unwrap(), b"heLlo\n");

        // A write extending past current EOF grows the file.
        pool.write(inode, 6, b"more").unwrap();
        assert_eq!(pool.read(inode, 0, 100).unwrap(), b"heLlo\nmore");
    }

    #[test]
    fn append_after_convert_is_rejected() {
        let t = TestPaths::new("noappendafterconvert", 2);
        let mut pool = Pool::format(&t.disks, &t.redb).unwrap();
        let inode = pool
            .create_file(ROOT_INODE, "f", any_n(2), 0, 0, 0o644)
            .unwrap();
        pool.append(inode, b"x".to_vec()).unwrap();
        pool.begin_convert(inode).unwrap();
        pool.complete_convert(inode).unwrap();
        assert!(matches!(
            pool.append(inode, b"y".to_vec()),
            Err(PoolError::WrongMode)
        ));
    }

    #[test]
    fn write_before_convert_is_rejected() {
        let t = TestPaths::new("nowritebeforeconvert", 2);
        let mut pool = Pool::format(&t.disks, &t.redb).unwrap();
        let inode = pool
            .create_file(ROOT_INODE, "f", any_n(2), 0, 0, 0o644)
            .unwrap();
        pool.append(inode, b"x".to_vec()).unwrap();
        assert!(matches!(
            pool.write(inode, 0, b"y"),
            Err(PoolError::WrongMode)
        ));
    }

    #[test]
    fn survives_reopen_after_crash() {
        let t = TestPaths::new("reopen", 3);
        let inode;
        {
            let mut pool = Pool::format(&t.disks, &t.redb).unwrap();
            inode = pool
                .create_file(ROOT_INODE, "f", any_n(2), 0, 0, 0o644)
                .unwrap();
            pool.append(inode, b"first".to_vec()).unwrap();
            pool.append(inode, b"second".to_vec()).unwrap();
            // Dropped here without any explicit shutdown/checkpoint —
            // simulates a crash right after the last successful append.
        }

        let pool2 = Pool::open(&t.disks, &t.redb).unwrap();
        assert_eq!(pool2.lookup(ROOT_INODE, "f").unwrap(), Some(inode));
        assert_eq!(pool2.read(inode, 0, 100).unwrap(), b"firstsecond");
    }

    #[test]
    fn replication_factor_places_on_distinct_disks() {
        let t = TestPaths::new("replicas", 4);
        let mut pool = Pool::format(&t.disks, &t.redb).unwrap();
        let inode = pool
            .create_file(ROOT_INODE, "f", any_n(3), 0, 0, 0o644)
            .unwrap();
        pool.append(inode, b"triplicated".to_vec()).unwrap();

        let rec = pool.get_inode(inode).unwrap().unwrap();
        let DataLocator::ChunkLog(chunks) = &rec.data else {
            panic!("expected ChunkLog")
        };
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].replicas.len(), 3);
        let mut disk_ids: Vec<_> = chunks[0].replicas.iter().map(|(id, _)| *id).collect();
        disk_ids.sort_by_key(|id| id.0);
        disk_ids.dedup();
        assert_eq!(
            disk_ids.len(),
            3,
            "3x replication must land on 3 distinct disks"
        );

        // Every replica must actually be independently readable and
        // agree on content.
        for (disk_id, offset) in &chunks[0].replicas {
            let handle = &pool.disks[disk_id];
            let mut buf = vec![0u8; b"triplicated".len()];
            handle
                .disk
                .read_at(
                    SEGMENT_BASE_OFFSET + offset + segment::HEADER_LEN as u64,
                    &mut buf,
                )
                .unwrap();
            assert_eq!(buf, b"triplicated");
        }
    }

    #[test]
    fn format_rejects_zero_disks() {
        let t = TestPaths::new("zerodisks", 0);
        assert!(matches!(
            Pool::format(&t.disks, &t.redb),
            Err(PoolError::NoDisks)
        ));
    }

    /// A single physical disk holding both the metadata WAL and file
    /// data (DESIGN.md §6's `min(2, disk count)` replication factor):
    /// the same create/append/read/convert/random-write shape as
    /// `convert_then_random_write_round_trips`, but on exactly 1 disk
    /// with a 1-replica scheme, since a 2-replica scheme can never be
    /// satisfied here.
    #[test]
    fn format_and_use_single_disk_pool() {
        let t = TestPaths::new("single", 1);
        let mut pool = Pool::format(&t.disks, &t.redb).unwrap();
        assert_eq!(pool.disk_count(), 1);

        // The root inode's own redundancy must already have scaled
        // down to 1 slot rather than being stuck asking for 2.
        let root = pool.get_inode(ROOT_INODE).unwrap().unwrap();
        match root.redundancy {
            RedundancyScheme::Replicated(slots) => assert_eq!(slots.len(), 1),
            other => panic!("expected Replicated, got {other:?}"),
        }

        let inode = pool
            .create_file(ROOT_INODE, "f", any_n(1), 0, 0, 0o644)
            .unwrap();
        pool.append(inode, b"hello\n".to_vec()).unwrap();
        assert_eq!(pool.read(inode, 0, 100).unwrap(), b"hello\n");

        pool.begin_convert(inode).unwrap();
        pool.complete_convert(inode).unwrap();
        assert_eq!(pool.read(inode, 0, 100).unwrap(), b"hello\n");

        pool.write(inode, 2, b"L").unwrap();
        assert_eq!(pool.read(inode, 0, 100).unwrap(), b"heLlo\n");
    }

    #[test]
    fn unlink_removes_directory_entry() {
        let t = TestPaths::new("unlink", 2);
        let mut pool = Pool::format(&t.disks, &t.redb).unwrap();
        let inode = pool
            .create_file(ROOT_INODE, "f", any_n(2), 0, 0, 0o644)
            .unwrap();
        assert_eq!(pool.lookup(ROOT_INODE, "f").unwrap(), Some(inode));
        pool.unlink(ROOT_INODE, "f").unwrap();
        assert_eq!(pool.lookup(ROOT_INODE, "f").unwrap(), None);
    }
}
