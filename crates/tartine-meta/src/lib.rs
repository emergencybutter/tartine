//! Metadata engine for the **FUSE prototype**: inode table, directory
//! tree, and the synchronous 2-disk WAL that replicates every mutation
//! before it is acknowledged. See DESIGN.md §6.
//!
//! The store itself (`Store` trait) is left abstract; the prototype backs
//! it with `redb` (a pure-Rust, transactional, file-backed B-tree — see
//! the workspace `Cargo.toml` comment). What's implemented concretely
//! here is the replication *protocol* around it, since that's the part
//! specific to this design rather than off-the-shelf. Production
//! (`kernel/`) reuses this same protocol but against its own in-kernel
//! on-disk B-tree instead of `redb` — see DESIGN.md §4.1, no userspace
//! embedded KV engine is usable from kernel context.

use std::io;

use tartine_core::disk::Disk;
use tartine_proto::{DiskId, MetaGroup, MetaOp};

/// The durable, ordered record of metadata mutations. `Store` is where a
/// real implementation would plug in `redb` (or `sled`) as the underlying
/// engine; this trait only asks for what the replication protocol below
/// needs from it.
pub trait Store {
    fn append_wal(&mut self, op: &MetaOp) -> io::Result<u64 /* wal offset */>;
    fn checkpoint(&mut self) -> io::Result<()>;
}

/// Drives the synchronous primary/backup replication described in
/// DESIGN.md §6: a `MetaOp` is not considered committed until it has been
/// durably written to every disk currently in the group.
pub struct MetaReplicator<D: Disk> {
    group: MetaGroup,
    primary_disk: D,
    backup_disk: Option<D>,
}

#[derive(Debug)]
pub enum CommitError {
    /// The backup failed; the caller should fence it out of the group
    /// (bump `epoch`, drop to primary-only) and continue — losing one
    /// replica degrades durability, it does not block writes.
    BackupUnavailable(io::Error),
    /// The primary failed. Unlike losing the backup, this is fatal to the
    /// current process's ability to serve the group at all; a real
    /// implementation would trigger promotion of the backup to primary
    /// and restart serving under a bumped epoch.
    PrimaryUnavailable(io::Error),
}

impl<D: Disk> MetaReplicator<D> {
    pub fn new(group: MetaGroup, primary_disk: D, backup_disk: Option<D>) -> Self {
        MetaReplicator {
            group,
            primary_disk,
            backup_disk,
        }
    }

    pub fn group(&self) -> MetaGroup {
        self.group
    }

    /// Commits one `MetaOp`: write to primary, then (if present) write to
    /// backup, only returning success once both are durable. Order
    /// matters for the failure semantics in DESIGN.md §12: a primary
    /// failure is escalated differently from a backup failure.
    pub fn commit(&mut self, encoded_op: &[u8], offset: u64) -> Result<(), CommitError> {
        self.primary_disk
            .write_at(offset, encoded_op)
            .and_then(|_| self.primary_disk.sync())
            .map_err(CommitError::PrimaryUnavailable)?;

        if let Some(backup) = &self.backup_disk {
            backup
                .write_at(offset, encoded_op)
                .and_then(|_| backup.sync())
                .map_err(CommitError::BackupUnavailable)?;
        }

        Ok(())
    }

    /// Fences the backup out after it's judged dead (DESIGN.md §6,
    /// "Failure"): bump the epoch and drop to primary-only so a stale
    /// backup can never be mistaken for current after coming back.
    pub fn fence_backup(&mut self) {
        self.group.backup = None;
        self.group.epoch += 1;
        self.backup_disk = None;
    }

    /// Adds (or replaces) the backup once a replacement disk has been
    /// fully resynced (DESIGN.md §6, "Planned migration" / post-failure
    /// catch-up) — the epoch bump makes this a distinct, unambiguous
    /// group generation from whatever came before.
    pub fn promote_new_backup(&mut self, disk_id: DiskId, disk: D) {
        self.group.backup = Some(disk_id);
        self.group.epoch += 1;
        self.backup_disk = Some(disk);
    }
}
