//! FUSE-facing layer: translates VFS operations (via `fuser` in a real
//! build) into the write-path state machine and metadata/data operations
//! defined in the lower crates. See DESIGN.md §9 and §11.

pub mod ioctl;
pub mod write_path;

use tartine_proto::{InodeMode, InodeRecord};

use crate::ioctl::{
    TartineState, TARTINE_MODE_APPEND_ONLY, TARTINE_MODE_CONVERTING, TARTINE_MODE_WRITABLE,
};

/// Handles `TARTINE_IOC_GET_STATE` (DESIGN.md §9.2): reports the inode's
/// current mode and, while `Converting`, how far the background
/// materializer has gotten.
pub fn get_state(inode: &InodeRecord, bytes_converted: u64) -> TartineState {
    let mode = match inode.mode {
        InodeMode::AppendOnly => TARTINE_MODE_APPEND_ONLY,
        InodeMode::Converting => TARTINE_MODE_CONVERTING,
        InodeMode::Writable => TARTINE_MODE_WRITABLE,
    };
    TartineState {
        mode,
        _pad: 0,
        bytes_total: inode.size,
        bytes_converted: if inode.mode == InodeMode::Converting {
            bytes_converted
        } else {
            inode.size
        },
    }
}

/// A real build implements `fuser::Filesystem` (or `fuse3`'s async
/// equivalent) directly; this struct is the shared state such an impl
/// would hold — kept dependency-free here so this crate compiles without
/// pulling `fuser` in. Its methods are the logical FUSE handlers, calling
/// straight into `write_path` and (eventually) `tartine-meta` /
/// `tartine-core`.
pub struct TartineFs {
    // inode table, disk handles, pool map, etc. live in tartine-meta /
    // tartine-core and are threaded in by `tartined`'s wiring.
}

impl TartineFs {
    /// Logical `write(2)` handler: classify the write, and hand off to
    /// the chunk-log append path or extent write path accordingly.
    /// Encoding to actual bytes on disk is `tartine-core`'s job; this is
    /// just the mode-driven decision plus the metadata transaction that
    /// follows a successful append (DESIGN.md §11).
    pub fn on_write(
        &mut self,
        inode: &mut InodeRecord,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, write_path::WriteError> {
        match write_path::classify_write(inode, offset, data.len())? {
            write_path::WriteKind::Append => {
                // tartine-core::placement::targets(..) picks replicas,
                // tartine-core::segment::SegmentWriter::append(..) writes
                // them, then a MetaOp::AppendChunk commits via
                // tartine-meta::MetaReplicator::commit(..).
                inode.size += data.len() as u64;
                Ok(data.len())
            }
            write_path::WriteKind::RandomWrite => {
                // extent resolve/allocate + read-modify-write; extent map
                // update is only a metadata txn when it *changes* the map.
                Ok(data.len())
            }
        }
    }

    /// Logical `ioctl(2)` handler for `TARTINE_IOC_MAKE_WRITABLE`
    /// (DESIGN.md §9.2). The actual background materialization
    /// (streaming read of the chunk-log, writing the new extent map) is
    /// `tartine-core`/`tartine-meta` work kicked off here, not shown.
    pub fn on_ioctl_make_writable(
        &mut self,
        inode: &mut InodeRecord,
        wait: bool,
    ) -> Result<(), write_path::ConvertError> {
        write_path::begin_convert(inode)?;
        if wait {
            // Synchronous variant: block here until the background
            // materializer finishes and calls complete_convert, instead
            // of returning immediately. Left as a hook point — the
            // actual scheduling lives with tartined's task runner.
        }
        Ok(())
    }
}
