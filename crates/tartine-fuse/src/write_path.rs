//! `InodeMode`-typed adapter over `tartine-kcore::write_path`. The FUSE
//! prototype's write handlers deal in `tartine_proto::InodeMode`; the
//! kernel module deals in the raw `u32` mode constants a C struct field
//! naturally holds. This module is the only place that translates
//! between the two — the decision logic itself lives once, in
//! `tartine-kcore`, and runs identically under both front ends. See
//! DESIGN.md §9, §11 and `tartine-kcore`'s crate doc comment.

use tartine_kcore::write_path as kcore;
use tartine_proto::{InodeMode, InodeRecord};

fn mode_to_u32(mode: InodeMode) -> u32 {
    match mode {
        InodeMode::AppendOnly => kcore::MODE_APPEND_ONLY,
        InodeMode::Converting => kcore::MODE_CONVERTING,
        InodeMode::Writable => kcore::MODE_WRITABLE,
    }
}

fn u32_to_mode(mode: u32) -> InodeMode {
    match mode {
        kcore::MODE_APPEND_ONLY => InodeMode::AppendOnly,
        kcore::MODE_CONVERTING => InodeMode::Converting,
        kcore::MODE_WRITABLE => InodeMode::Writable,
        _ => unreachable!("tartine-kcore only ever hands back its own MODE_* constants"),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum WriteError {
    NotAppendOnlyOrder,
    RandomWriteDenied,
    ConversionInProgress,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WriteKind {
    Append,
    RandomWrite,
}

pub fn classify_write(
    inode: &InodeRecord,
    offset: u64,
    _len: usize,
) -> Result<WriteKind, WriteError> {
    match kcore::tartine_classify_write(mode_to_u32(inode.mode), inode.size, offset) {
        kcore::WRITE_KIND_APPEND => Ok(WriteKind::Append),
        kcore::WRITE_KIND_RANDOM => Ok(WriteKind::RandomWrite),
        kcore::ERR_NOT_APPEND_ORDER => Err(WriteError::NotAppendOnlyOrder),
        kcore::ERR_CONVERSION_IN_PROGRESS => Err(WriteError::ConversionInProgress),
        other => unreachable!("tartine-kcore returned unexpected code {other} for a valid mode"),
    }
}

pub fn truncate_allowed(inode: &InodeRecord) -> Result<(), WriteError> {
    match kcore::tartine_truncate_allowed(mode_to_u32(inode.mode)) {
        0 => Ok(()),
        kcore::ERR_RANDOM_WRITE_DENIED => Err(WriteError::RandomWriteDenied),
        other => unreachable!("tartine-kcore returned unexpected code {other} for a valid mode"),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConvertError {
    AlreadyWritable,
    AlreadyConverting,
}

pub fn begin_convert(inode: &mut InodeRecord) -> Result<(), ConvertError> {
    match kcore::tartine_begin_convert(mode_to_u32(inode.mode)) {
        kcore::ERR_ALREADY_WRITABLE => Err(ConvertError::AlreadyWritable),
        kcore::ERR_ALREADY_CONVERTING => Err(ConvertError::AlreadyConverting),
        new_mode if new_mode >= 0 => {
            inode.mode = u32_to_mode(new_mode as u32);
            Ok(())
        }
        other => unreachable!("tartine-kcore returned unexpected code {other} for a valid mode"),
    }
}

pub fn complete_convert(inode: &mut InodeRecord, extents: Vec<tartine_proto::Extent>) {
    let new_mode = kcore::tartine_complete_convert(mode_to_u32(inode.mode));
    assert!(new_mode >= 0, "complete_convert called out of order");
    inode.data = tartine_proto::DataLocator::Extents(extents);
    inode.mode = u32_to_mode(new_mode as u32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tartine_proto::{DataLocator, RedundancyScheme, ReplicaSlot};

    fn append_only_inode(size: u64) -> InodeRecord {
        InodeRecord {
            inode: 1,
            mode: InodeMode::AppendOnly,
            size,
            redundancy: RedundancyScheme::Replicated(vec![ReplicaSlot::AnyOfClass(None); 2]),
            data: DataLocator::ChunkLog(vec![]),
            uid: 0,
            gid: 0,
            unix_mode: 0o644,
            mtime_unix: 0,
        }
    }

    #[test]
    fn append_only_accepts_write_at_eof() {
        let inode = append_only_inode(100);
        assert!(matches!(
            classify_write(&inode, 100, 10),
            Ok(WriteKind::Append)
        ));
    }

    #[test]
    fn append_only_rejects_write_before_eof() {
        let inode = append_only_inode(100);
        assert_eq!(
            classify_write(&inode, 50, 10),
            Err(WriteError::NotAppendOnlyOrder)
        );
    }

    #[test]
    fn append_only_rejects_truncate() {
        let inode = append_only_inode(100);
        assert_eq!(truncate_allowed(&inode), Err(WriteError::RandomWriteDenied));
    }

    #[test]
    fn converting_still_accepts_tail_append_but_not_random_write() {
        let mut inode = append_only_inode(100);
        begin_convert(&mut inode).unwrap();
        assert!(matches!(
            classify_write(&inode, 100, 10),
            Ok(WriteKind::Append)
        ));
        assert_eq!(
            classify_write(&inode, 10, 10),
            Err(WriteError::ConversionInProgress)
        );
    }

    #[test]
    fn convert_is_one_way() {
        let mut inode = append_only_inode(100);
        begin_convert(&mut inode).unwrap();
        complete_convert(&mut inode, vec![]);
        assert_eq!(inode.mode, InodeMode::Writable);
        assert_eq!(
            begin_convert(&mut inode),
            Err(ConvertError::AlreadyWritable)
        );

        assert!(matches!(
            classify_write(&inode, 0, 10),
            Ok(WriteKind::RandomWrite)
        ));
        assert!(truncate_allowed(&inode).is_ok());
    }
}
