//! The two write paths (DESIGN.md §9, §11) and the state machine that
//! decides which one applies. This is the part of the design most worth
//! having as real, testable logic rather than a stub, since it's the
//! crux of "born append-only, convertible to writable".

use tartine_proto::{InodeMode, InodeRecord};

#[derive(Debug, PartialEq, Eq)]
pub enum WriteError {
    /// `AppendOnly` file, write offset wasn't at EOF.
    NotAppendOnlyOrder,
    /// `AppendOnly` file, caller tried `ftruncate`/random `pwrite`.
    RandomWriteDenied,
    /// `Converting` file, a random write landed before the atomic swap
    /// to `Writable` could make it safe to accept (DESIGN.md §9.3 step 4).
    ConversionInProgress,
}

/// What kind of write, if any, `offset`/`len` against `inode` resolves to.
/// Mirrors the FUSE `write` handler's first decision.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteKind {
    /// Append to the chunk-log (DESIGN.md §9.1).
    Append,
    /// Ordinary in-place write against the extent map (DESIGN.md §11).
    RandomWrite,
}

pub fn classify_write(
    inode: &InodeRecord,
    offset: u64,
    _len: usize,
) -> Result<WriteKind, WriteError> {
    match inode.mode {
        InodeMode::AppendOnly => {
            if offset == inode.size {
                Ok(WriteKind::Append)
            } else {
                Err(WriteError::NotAppendOnlyOrder)
            }
        }
        InodeMode::Converting => {
            // Appends past the point the materializer already captured
            // are still accepted (DESIGN.md §9.3 step 4); anything else
            // has to wait for the swap to `Writable`.
            if offset == inode.size {
                Ok(WriteKind::Append)
            } else {
                Err(WriteError::ConversionInProgress)
            }
        }
        InodeMode::Writable => Ok(WriteKind::RandomWrite),
    }
}

pub fn truncate_allowed(inode: &InodeRecord) -> Result<(), WriteError> {
    match inode.mode {
        InodeMode::AppendOnly | InodeMode::Converting => Err(WriteError::RandomWriteDenied),
        InodeMode::Writable => Ok(()),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConvertError {
    /// Already `Writable`; conversion is one-way (DESIGN.md §9.3).
    AlreadyWritable,
    /// A conversion is already in flight for this inode.
    AlreadyConverting,
}

/// Step 1 of DESIGN.md §9.3: flip the inode to `Converting`. Cheap — this
/// is what lets the async ioctl variant return immediately.
pub fn begin_convert(inode: &mut InodeRecord) -> Result<(), ConvertError> {
    match inode.mode {
        InodeMode::Writable => Err(ConvertError::AlreadyWritable),
        InodeMode::Converting => Err(ConvertError::AlreadyConverting),
        InodeMode::AppendOnly => {
            inode.mode = InodeMode::Converting;
            Ok(())
        }
    }
}

/// Step 5 of DESIGN.md §9.3: the atomic metadata swap once materialization
/// has copied all data into the new extent map. Infallible by
/// construction — by the time the background job calls this, the extent
/// map is already fully written and checksummed; there is nothing left to
/// fail here except a logic bug upstream, which we assert against instead
/// of threading a new error variant through callers that can't act on it.
pub fn complete_convert(inode: &mut InodeRecord, extents: Vec<tartine_proto::Extent>) {
    assert_eq!(
        inode.mode,
        InodeMode::Converting,
        "complete_convert called out of order"
    );
    inode.data = tartine_proto::DataLocator::Extents(extents);
    inode.mode = InodeMode::Writable;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tartine_proto::DataLocator;

    fn append_only_inode(size: u64) -> InodeRecord {
        InodeRecord {
            inode: 1,
            mode: InodeMode::AppendOnly,
            size,
            replication_factor: 2,
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

        // and once writable, arbitrary offsets + truncate are fine
        assert!(matches!(
            classify_write(&inode, 0, 10),
            Ok(WriteKind::RandomWrite)
        ));
        assert!(truncate_allowed(&inode).is_ok());
    }
}
