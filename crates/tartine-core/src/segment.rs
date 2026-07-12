//! Chunk-log segment format for append-only file data (DESIGN.md §5.3).
//!
//! Segments are written strictly sequentially and sealed once full; a
//! background compactor (not modeled here) is what reclaims space from
//! records superseded by delete/truncate/conversion.

use tartine_proto::InodeId;

pub const SEGMENT_SIZE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct SegmentHeader {
    pub segment_id: u64,
    pub created_at_unix: u64,
}

/// One physical record in a segment: an append for a single inode.
#[derive(Debug, Clone)]
pub struct Record {
    pub inode: InodeId,
    pub chunk_seq: u64,
    pub checksum: u64,
    pub payload: Vec<u8>,
}

/// Owns the write cursor for one open (not-yet-sealed) segment on one
/// disk. `append` is the only mutating operation, matching the
/// append-only-by-construction property this format exists to provide.
pub struct SegmentWriter<D: crate::disk::Disk> {
    disk: D,
    base_offset: u64,
    cursor: u64,
}

impl<D: crate::disk::Disk> SegmentWriter<D> {
    pub fn new(disk: D, base_offset: u64) -> Self {
        SegmentWriter {
            disk,
            base_offset,
            cursor: 0,
        }
    }

    pub fn remaining(&self) -> u64 {
        SEGMENT_SIZE_BYTES.saturating_sub(self.cursor)
    }

    /// Appends `record`, returning the offset it was written at (used to
    /// build the `ChunkPointer` stored in metadata). Fails if the record
    /// doesn't fit in the remaining space of this segment; the caller
    /// rolls over to a fresh segment in that case.
    pub fn append(&mut self, record: &Record) -> std::io::Result<u64> {
        let encoded = encode(record);
        if encoded.len() as u64 > self.remaining() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "segment full",
            ));
        }
        let offset = self.base_offset + self.cursor;
        self.disk.write_at(offset, &encoded)?;
        self.cursor += encoded.len() as u64;
        Ok(offset)
    }

    pub fn sync(&self) -> std::io::Result<()> {
        self.disk.sync()
    }
}

/// Minimal length-prefixed encoding: `inode(8) chunk_seq(8) checksum(8)
/// len(4) payload(len)`. A real implementation would use `bincode`/`serde`
/// (see the workspace `Cargo.toml` comment) — spelled out by hand here so
/// this crate has zero external dependencies.
fn encode(record: &Record) -> Vec<u8> {
    let mut buf = Vec::with_capacity(28 + record.payload.len());
    buf.extend_from_slice(&record.inode.to_le_bytes());
    buf.extend_from_slice(&record.chunk_seq.to_le_bytes());
    buf.extend_from_slice(&record.checksum.to_le_bytes());
    buf.extend_from_slice(&(record.payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&record.payload);
    buf
}
