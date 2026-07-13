//! `MetaStore`: the thing `Pool`/`TartineFs` actually talk to
//! (IMPLEMENTATION_PLAN.md P1.3). Composes two pieces that stay
//! separately testable:
//!
//! - **Durability**: every `MetaOp` is encoded (`codec`), framed
//!   (`wal`), and committed through the existing, already-tested
//!   `MetaReplicator` — synchronously written to both metadata disks
//!   before the call returns, with DESIGN.md §6's fencing behavior on
//!   backup failure. This is the load-bearing durability guarantee.
//! - **Fast reads**: a local `redb` database holding the *materialized*
//!   current state (inode table, directory tree), rebuilt at open time
//!   by replaying the WAL. This is a rebuildable cache, not itself one
//!   of the two durable copies — losing it (e.g. disk corruption on the
//!   local cache file only, not the metadata disks) just means a slower
//!   reopen, not data loss. DESIGN.md §6 step 4 describes periodically
//!   checkpointing the WAL into the B-tree on *both* metadata disks and
//!   truncating it; that's not implemented here (no checkpoint/truncate
//!   yet — WAL replay-at-open is always correct, just unboundedly slow
//!   for a very long-lived mount, which is a performance concern the
//!   prototype phase explicitly defers, not a correctness one).

use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use tartine_core::disk::Disk;
use tartine_proto::{DataLocator, InodeId, InodeMode, InodeRecord, MetaGroup, MetaOp};

use crate::codec::{self, Decoder, Encoder};
use crate::wal;
use crate::{CommitError, MetaReplicator};

const INODES: TableDefinition<u64, &[u8]> = TableDefinition::new("inodes");
/// Key = `parent_inode (8 bytes LE) ++ name (UTF-8 bytes)`, value =
/// child inode id. A composite key as raw bytes rather than a redb
/// multi-column key — simplest thing that works and keeps `codec.rs`'s
/// "everything is bytes" style consistent across this crate.
const DIR_ENTRIES: TableDefinition<&[u8], u64> = TableDefinition::new("dir_entries");

fn dir_key(parent: InodeId, name: &str) -> Vec<u8> {
    let mut key = parent.to_le_bytes().to_vec();
    key.extend_from_slice(name.as_bytes());
    key
}

#[derive(Debug)]
pub enum MetaStoreError {
    Commit(CommitError),
    Redb(String),
    Io(String),
    /// The op assumed a precondition this store doesn't (yet) enforce
    /// server-side — e.g. `AppendChunk` against a file that's already
    /// `Writable`. Kept distinct from `Redb`/`Commit` so callers
    /// (`TartineFs`) can map it to the right `errno`.
    InvalidOp(&'static str),
}

impl std::fmt::Display for MetaStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetaStoreError::Commit(e) => write!(f, "WAL commit failed: {e:?}"),
            MetaStoreError::Redb(e) => write!(f, "local store error: {e}"),
            MetaStoreError::Io(e) => write!(f, "I/O error: {e}"),
            MetaStoreError::InvalidOp(msg) => write!(f, "invalid metadata operation: {msg}"),
        }
    }
}
impl std::error::Error for MetaStoreError {}

impl From<redb::Error> for MetaStoreError {
    fn from(e: redb::Error) -> Self {
        MetaStoreError::Redb(e.to_string())
    }
}
impl From<redb::TransactionError> for MetaStoreError {
    fn from(e: redb::TransactionError) -> Self {
        MetaStoreError::Redb(e.to_string())
    }
}
impl From<redb::TableError> for MetaStoreError {
    fn from(e: redb::TableError) -> Self {
        MetaStoreError::Redb(e.to_string())
    }
}
impl From<redb::StorageError> for MetaStoreError {
    fn from(e: redb::StorageError) -> Self {
        MetaStoreError::Redb(e.to_string())
    }
}
impl From<redb::CommitError> for MetaStoreError {
    fn from(e: redb::CommitError) -> Self {
        MetaStoreError::Redb(e.to_string())
    }
}
impl From<redb::DatabaseError> for MetaStoreError {
    fn from(e: redb::DatabaseError) -> Self {
        MetaStoreError::Redb(e.to_string())
    }
}
impl From<std::io::Error> for MetaStoreError {
    fn from(e: std::io::Error) -> Self {
        MetaStoreError::Io(e.to_string())
    }
}

pub struct MetaStore<D: Disk> {
    replicator: MetaReplicator<D>,
    wal_base_offset: u64,
    wal_cursor: u64,
    db: Database,
    /// Cached separately from redb (rather than re-scanning `INODES` for
    /// the max key on every allocation) — trivial to keep correct
    /// alongside `db` since the only way an inode gets created is
    /// through `apply(MetaOp::CreateInode)`, which updates both.
    next_inode: InodeId,
}

impl<D: Disk> MetaStore<D> {
    /// Opens (or initializes, if the WAL is empty) a metadata store: WAL
    /// replication lives on `primary`/`backup`; the local materialized
    /// view lives at `redb_path`, deleted and rebuilt from a full WAL
    /// replay every time (simplest correct thing — no attempt to detect
    /// whether an existing `redb_path` is already caught up, since
    /// replay is cheap at prototype scale and "always rebuild" can never
    /// be stale).
    pub fn open(
        group: MetaGroup,
        primary: D,
        backup: Option<D>,
        redb_path: &Path,
    ) -> Result<Self, MetaStoreError> {
        let scan = wal::scan(&primary, 0)?;

        let _ = std::fs::remove_file(redb_path);
        let db = Database::create(redb_path)?;
        {
            let txn = db.begin_write()?;
            txn.open_table(INODES)?;
            txn.open_table(DIR_ENTRIES)?;
            txn.commit()?;
        }

        let mut store = MetaStore {
            replicator: MetaReplicator::new(group, primary, backup),
            wal_base_offset: 0,
            wal_cursor: scan.resume_at,
            db,
            next_inode: 1,
        };

        for payload in &scan.entries {
            let op = codec::decode_op(payload);
            store.apply_locally(&op)?;
        }

        Ok(store)
    }

    pub fn group(&self) -> MetaGroup {
        self.replicator.group()
    }

    pub fn alloc_inode(&mut self) -> InodeId {
        let id = self.next_inode;
        self.next_inode += 1;
        id
    }

    /// Commits `op` durably (WAL, replicated to both metadata disks —
    /// DESIGN.md §6) and then applies it to the local materialized view.
    /// Order matters: an op is never visible to reads until it's
    /// durable, matching DESIGN.md §11's "metadata txn" ordering.
    pub fn apply(&mut self, op: MetaOp) -> Result<(), MetaStoreError> {
        let payload = codec::encode_op(&op);
        let frame = wal::build_frame(&payload);
        let offset = self.wal_base_offset + self.wal_cursor;

        match self.replicator.commit(&frame, offset) {
            Ok(()) => {}
            Err(CommitError::BackupUnavailable(_)) => {
                // Degraded but not fatal (DESIGN.md §6): the op is
                // durable on the primary, so it's safe to keep going.
                self.replicator.fence_backup();
            }
            Err(e @ CommitError::PrimaryUnavailable(_)) => return Err(MetaStoreError::Commit(e)),
        }
        self.wal_cursor += frame.len() as u64;

        self.apply_locally(&op)
    }

    fn apply_locally(&mut self, op: &MetaOp) -> Result<(), MetaStoreError> {
        let txn = self.db.begin_write()?;
        {
            let mut inodes = txn.open_table(INODES)?;
            let mut dirs = txn.open_table(DIR_ENTRIES)?;

            match op {
                MetaOp::CreateInode(rec) => {
                    self.next_inode = self.next_inode.max(rec.inode + 1);
                    let mut e = Encoder::new();
                    codec::encode_inode_record(&mut e, rec);
                    inodes.insert(rec.inode, e.into_vec().as_slice())?;
                }
                MetaOp::Link {
                    parent,
                    name,
                    child,
                } => {
                    dirs.insert(dir_key(*parent, name).as_slice(), *child)?;
                }
                MetaOp::Unlink { parent, name } => {
                    dirs.remove(dir_key(*parent, name).as_slice())?;
                }
                MetaOp::AppendChunk { inode, chunk } => {
                    let mut rec = read_inode(&inodes, *inode)?;
                    match &mut rec.data {
                        DataLocator::ChunkLog(chunks) => chunks.push(chunk.clone()),
                        DataLocator::Extents(_) => {
                            return Err(MetaStoreError::InvalidOp(
                                "AppendChunk against a Writable (extent-mapped) file",
                            ))
                        }
                    }
                    rec.size += chunk.len as u64;
                    write_inode(&mut inodes, &rec)?;
                }
                MetaOp::BeginConvert { inode } => {
                    let mut rec = read_inode(&inodes, *inode)?;
                    let new_mode =
                        tartine_kcore::write_path::tartine_begin_convert(mode_to_u32(rec.mode));
                    if new_mode < 0 {
                        return Err(MetaStoreError::InvalidOp(
                            "BeginConvert on a file that's already Converting or Writable",
                        ));
                    }
                    rec.mode = u32_to_mode(new_mode as u32);
                    write_inode(&mut inodes, &rec)?;
                }
                MetaOp::CompleteConvert { inode, extents } => {
                    let mut rec = read_inode(&inodes, *inode)?;
                    let new_mode =
                        tartine_kcore::write_path::tartine_complete_convert(mode_to_u32(rec.mode));
                    if new_mode < 0 {
                        return Err(MetaStoreError::InvalidOp(
                            "CompleteConvert called out of order",
                        ));
                    }
                    rec.mode = u32_to_mode(new_mode as u32);
                    rec.data = DataLocator::Extents(extents.clone());
                    write_inode(&mut inodes, &rec)?;
                }
                MetaOp::SetRedundancyScheme { inode, scheme } => {
                    let mut rec = read_inode(&inodes, *inode)?;
                    rec.redundancy = scheme.clone();
                    write_inode(&mut inodes, &rec)?;
                }
                MetaOp::UpdateExtents { inode, extents } => {
                    let mut rec = read_inode(&inodes, *inode)?;
                    if rec.mode != InodeMode::Writable {
                        return Err(MetaStoreError::InvalidOp(
                            "UpdateExtents against a file that isn't Writable yet",
                        ));
                    }
                    rec.size = extents
                        .iter()
                        .map(|x| x.file_offset + x.len as u64)
                        .max()
                        .unwrap_or(0);
                    rec.data = DataLocator::Extents(extents.clone());
                    write_inode(&mut inodes, &rec)?;
                }
                MetaOp::PoolMapChange(_) | MetaOp::MetaGroupChange(_) => {
                    // Not consumed here: `Pool` (IMPLEMENTATION_PLAN.md
                    // P1.4) holds the live PoolMap/MetaGroup in memory
                    // directly. These variants are still durably logged
                    // (the WAL commit above already happened) so a
                    // future rebalancer/disk-add-remove implementation
                    // (P1.7) can replay them; there's just no local
                    // materialized-view table for them yet.
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_inode(&self, id: InodeId) -> Result<Option<InodeRecord>, MetaStoreError> {
        let txn = self.db.begin_read()?;
        let inodes = txn.open_table(INODES)?;
        match inodes.get(id)? {
            Some(guard) => {
                let mut d = Decoder::new(guard.value());
                Ok(Some(codec::decode_inode_record(&mut d)))
            }
            None => Ok(None),
        }
    }

    pub fn lookup(&self, parent: InodeId, name: &str) -> Result<Option<InodeId>, MetaStoreError> {
        let txn = self.db.begin_read()?;
        let dirs = txn.open_table(DIR_ENTRIES)?;
        Ok(dirs
            .get(dir_key(parent, name).as_slice())?
            .map(|g| g.value()))
    }

    pub fn readdir(&self, parent: InodeId) -> Result<Vec<(String, InodeId)>, MetaStoreError> {
        let txn = self.db.begin_read()?;
        let dirs = txn.open_table(DIR_ENTRIES)?;
        let prefix = parent.to_le_bytes();
        let mut out = Vec::new();
        for entry in dirs.range::<&[u8]>(prefix.as_slice()..)? {
            let (key, value) = entry?;
            let key = key.value();
            if !key.starts_with(&prefix) {
                break;
            }
            let name = String::from_utf8(key[8..].to_vec())
                .map_err(|e| MetaStoreError::Redb(e.to_string()))?;
            out.push((name, value.value()));
        }
        Ok(out)
    }
}

fn read_inode(
    table: &redb::Table<u64, &'static [u8]>,
    id: InodeId,
) -> Result<InodeRecord, MetaStoreError> {
    let bytes = table
        .get(id)?
        .ok_or(MetaStoreError::InvalidOp(
            "operation referenced an inode that doesn't exist",
        ))?
        .value()
        .to_vec();
    let mut d = Decoder::new(&bytes);
    Ok(codec::decode_inode_record(&mut d))
}

fn write_inode(
    table: &mut redb::Table<u64, &[u8]>,
    rec: &InodeRecord,
) -> Result<(), MetaStoreError> {
    let mut e = Encoder::new();
    codec::encode_inode_record(&mut e, rec);
    table.insert(rec.inode, e.into_vec().as_slice())?;
    Ok(())
}

fn mode_to_u32(m: InodeMode) -> u32 {
    match m {
        InodeMode::AppendOnly => tartine_kcore::write_path::MODE_APPEND_ONLY,
        InodeMode::Converting => tartine_kcore::write_path::MODE_CONVERTING,
        InodeMode::Writable => tartine_kcore::write_path::MODE_WRITABLE,
    }
}
fn u32_to_mode(m: u32) -> InodeMode {
    match m {
        tartine_kcore::write_path::MODE_APPEND_ONLY => InodeMode::AppendOnly,
        tartine_kcore::write_path::MODE_CONVERTING => InodeMode::Converting,
        tartine_kcore::write_path::MODE_WRITABLE => InodeMode::Writable,
        other => {
            unreachable!("tartine-kcore only ever returns its own MODE_* constants, got {other}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tartine_core::disk::FileDisk;
    use tartine_proto::{DataLocator, RedundancyScheme, ReplicaSlot, Uuid};

    /// Wraps a `FileDisk` with a toggleable "always fail" switch, so a
    /// single `MetaStore<D>` (generic over one disk type for both
    /// primary and backup) can still exercise "the backup disk becomes
    /// unavailable" without needing two different `Disk` impls.
    struct FlakyDisk {
        inner: FileDisk,
        fail: AtomicBool,
    }
    impl FlakyDisk {
        fn new(inner: FileDisk, fail: bool) -> Self {
            FlakyDisk {
                inner,
                fail: AtomicBool::new(fail),
            }
        }
    }
    impl Disk for FlakyDisk {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(io::Error::other("simulated disk failure"));
            }
            self.inner.read_at(offset, buf)
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(io::Error::other("simulated disk failure"));
            }
            self.inner.write_at(offset, buf)
        }
        fn sync(&self) -> io::Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(io::Error::other("simulated disk failure"));
            }
            self.inner.sync()
        }
        fn capacity_bytes(&self) -> u64 {
            self.inner.capacity_bytes()
        }
    }

    fn tempfile_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "tartine-metastore-test-{}-{}",
            std::process::id(),
            name
        ));
        p
    }

    fn root_record() -> InodeRecord {
        InodeRecord {
            inode: 1,
            mode: InodeMode::Writable,
            size: 0,
            redundancy: RedundancyScheme::Replicated(vec![ReplicaSlot::AnyOfClass(None); 2]),
            data: DataLocator::Extents(vec![]),
            uid: 0,
            gid: 0,
            unix_mode: 0o755,
            mtime_unix: 0,
        }
    }

    fn open_pair(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        (
            tempfile_path(&format!("{name}-primary")),
            tempfile_path(&format!("{name}-backup")),
            tempfile_path(&format!("{name}-redb")),
        )
    }

    #[test]
    fn create_link_and_read_back() {
        let (p, b, r) = open_pair("basic");
        for path in [&p, &b, &r] {
            let _ = std::fs::remove_file(path);
        }
        let primary = FileDisk::create(&p, wal::WAL_CAPACITY_BYTES).unwrap();
        let backup = FileDisk::create(&b, wal::WAL_CAPACITY_BYTES).unwrap();
        let group = MetaGroup {
            primary: Uuid(1),
            backup: Some(Uuid(2)),
            epoch: 0,
        };

        let mut store = MetaStore::open(group, primary, Some(backup), &r).unwrap();
        store.apply(MetaOp::CreateInode(root_record())).unwrap();

        let child_id = store.alloc_inode();
        let mut child = root_record();
        child.inode = child_id;
        child.mode = InodeMode::AppendOnly;
        child.data = DataLocator::ChunkLog(vec![]);
        store.apply(MetaOp::CreateInode(child)).unwrap();
        store
            .apply(MetaOp::Link {
                parent: 1,
                name: "f".into(),
                child: child_id,
            })
            .unwrap();

        assert_eq!(store.lookup(1, "f").unwrap(), Some(child_id));
        assert_eq!(store.readdir(1).unwrap(), vec![("f".to_string(), child_id)]);
        assert_eq!(
            store.get_inode(child_id).unwrap().unwrap().mode,
            InodeMode::AppendOnly
        );

        for path in [&p, &b, &r] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn survives_reopen_via_wal_replay() {
        let (p, b, r) = open_pair("reopen");
        for path in [&p, &b, &r] {
            let _ = std::fs::remove_file(path);
        }
        let group = MetaGroup {
            primary: Uuid(1),
            backup: Some(Uuid(2)),
            epoch: 0,
        };
        {
            let primary = FileDisk::create(&p, wal::WAL_CAPACITY_BYTES).unwrap();
            let backup = FileDisk::create(&b, wal::WAL_CAPACITY_BYTES).unwrap();
            let mut store = MetaStore::open(group, primary, Some(backup), &r).unwrap();
            store.apply(MetaOp::CreateInode(root_record())).unwrap();
            let child_id = store.alloc_inode();
            let mut child = root_record();
            child.inode = child_id;
            store.apply(MetaOp::CreateInode(child)).unwrap();
            store
                .apply(MetaOp::Link {
                    parent: 1,
                    name: "persisted".into(),
                    child: child_id,
                })
                .unwrap();
        }

        // Reopen against the *same* primary/backup files — this is the
        // crash-recovery path: nothing but the WAL on disk is trusted.
        let primary2 = FileDisk::open(&p).unwrap();
        let backup2 = FileDisk::open(&b).unwrap();
        let store2 = MetaStore::open(group, primary2, Some(backup2), &r).unwrap();
        assert!(store2.lookup(1, "persisted").unwrap().is_some());

        for path in [&p, &b, &r] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn append_chunk_updates_size_and_chunk_log() {
        let (p, b, r) = open_pair("append");
        for path in [&p, &b, &r] {
            let _ = std::fs::remove_file(path);
        }
        let group = MetaGroup {
            primary: Uuid(1),
            backup: Some(Uuid(2)),
            epoch: 0,
        };
        let primary = FileDisk::create(&p, wal::WAL_CAPACITY_BYTES).unwrap();
        let backup = FileDisk::create(&b, wal::WAL_CAPACITY_BYTES).unwrap();
        let mut store = MetaStore::open(group, primary, Some(backup), &r).unwrap();

        let mut file = root_record();
        file.inode = 2;
        file.mode = InodeMode::AppendOnly;
        file.size = 0;
        file.data = DataLocator::ChunkLog(vec![]);
        store.apply(MetaOp::CreateInode(file)).unwrap();

        store
            .apply(MetaOp::AppendChunk {
                inode: 2,
                chunk: tartine_proto::ChunkPointer {
                    chunk_seq: 0,
                    replicas: vec![(Uuid(9), 0)],
                    len: 5,
                    checksum: 1,
                },
            })
            .unwrap();

        let rec = store.get_inode(2).unwrap().unwrap();
        assert_eq!(rec.size, 5);
        match rec.data {
            DataLocator::ChunkLog(chunks) => assert_eq!(chunks.len(), 1),
            _ => panic!("expected ChunkLog"),
        }

        for path in [&p, &b, &r] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn convert_transitions_mode_and_data_locator() {
        let (p, b, r) = open_pair("convert");
        for path in [&p, &b, &r] {
            let _ = std::fs::remove_file(path);
        }
        let group = MetaGroup {
            primary: Uuid(1),
            backup: Some(Uuid(2)),
            epoch: 0,
        };
        let primary = FileDisk::create(&p, wal::WAL_CAPACITY_BYTES).unwrap();
        let backup = FileDisk::create(&b, wal::WAL_CAPACITY_BYTES).unwrap();
        let mut store = MetaStore::open(group, primary, Some(backup), &r).unwrap();

        let mut file = root_record();
        file.inode = 2;
        file.mode = InodeMode::AppendOnly;
        file.data = DataLocator::ChunkLog(vec![]);
        store.apply(MetaOp::CreateInode(file)).unwrap();

        store.apply(MetaOp::BeginConvert { inode: 2 }).unwrap();
        assert_eq!(
            store.get_inode(2).unwrap().unwrap().mode,
            InodeMode::Converting
        );

        store
            .apply(MetaOp::CompleteConvert {
                inode: 2,
                extents: vec![],
            })
            .unwrap();
        let rec = store.get_inode(2).unwrap().unwrap();
        assert_eq!(rec.mode, InodeMode::Writable);
        assert!(matches!(rec.data, DataLocator::Extents(_)));

        for path in [&p, &b, &r] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn backup_failure_fences_but_keeps_serving_degraded() {
        let (p, b, r) = open_pair("fence");
        for path in [&p, &b, &r] {
            let _ = std::fs::remove_file(path);
        }
        let group = MetaGroup {
            primary: Uuid(1),
            backup: Some(Uuid(2)),
            epoch: 0,
        };
        let primary = FlakyDisk::new(
            FileDisk::create(&p, wal::WAL_CAPACITY_BYTES).unwrap(),
            false,
        );
        let backup = FlakyDisk::new(FileDisk::create(&b, wal::WAL_CAPACITY_BYTES).unwrap(), true);
        let mut store = MetaStore::open(group, primary, Some(backup), &r).unwrap();
        assert!(store.group().backup.is_some());
        assert_eq!(store.group().epoch, 0);

        // The backup is failing, but DESIGN.md §6 says the op is still
        // durable (on the primary) and the write is NOT rejected —
        // degraded, not blocked.
        store.apply(MetaOp::CreateInode(root_record())).unwrap();

        assert!(
            store.group().backup.is_none(),
            "backup should be fenced out after failing"
        );
        assert_eq!(store.group().epoch, 1, "fencing must bump the epoch");
        assert!(
            store.get_inode(1).unwrap().is_some(),
            "the op that triggered fencing still applied"
        );

        // Continuing to apply ops afterward (primary-only) must keep
        // working — degraded mode isn't a dead end.
        let child_id = store.alloc_inode();
        let mut child = root_record();
        child.inode = child_id;
        store.apply(MetaOp::CreateInode(child)).unwrap();
        store
            .apply(MetaOp::Link {
                parent: 1,
                name: "still-works".into(),
                child: child_id,
            })
            .unwrap();
        assert_eq!(store.lookup(1, "still-works").unwrap(), Some(child_id));

        for path in [&p, &b, &r] {
            let _ = std::fs::remove_file(path);
        }
    }
}
