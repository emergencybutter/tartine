//! Chunk-log segment format for append-only file data (DESIGN.md §5.3).
//!
//! Segments are written strictly sequentially and sealed once full; a
//! background compactor (not modeled here) is what reclaims space from
//! records superseded by delete/truncate/conversion.

use std::io;

use tartine_proto::InodeId;

pub const SEGMENT_SIZE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct SegmentHeader {
    pub segment_id: u64,
    pub created_at_unix: u64,
}

/// Marks the start of a record. Its only job is to make a genuine record
/// distinguishable from unwritten (zero-filled, since backing files are
/// created sparse — `disk::FileDisk::create`) space: an all-zero region
/// decodes as `inode=0, chunk_seq=0, checksum=0, len=0`, which is a
/// *self-consistent* empty record (crc32c of zero bytes is 0 — see
/// `crc32c::tests::empty_input_is_zero`), so without a magic value a
/// reader can't tell "nothing was ever written here" from "an empty
/// record was written here." Chosen to be vanishingly unlikely to occur
/// by coincidence in zeroed or arbitrary data.
const RECORD_MAGIC: u32 = 0x5441_5243; // "TARC" in ASCII, arbitrary but fixed

/// `magic(4) inode(8) chunk_seq(8) checksum(4) len(4)`.
/// `magic(4) inode(8) chunk_seq(8) checksum(4) len(4)` — public so a
/// reader that already knows a record's offset (from its
/// `ChunkPointer`, DESIGN.md §5.3) can seek straight to the payload
/// instead of re-scanning from the segment start.
pub const HEADER_LEN: usize = 4 + 8 + 8 + 4 + 4;

/// One physical record in a segment: an append for a single inode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub inode: InodeId,
    pub chunk_seq: u64,
    /// crc32c of `payload` (DESIGN.md §5.3).
    pub checksum: u32,
    pub payload: Vec<u8>,
}

impl Record {
    /// Builds a record with its checksum computed from `payload`, so
    /// normal callers can't forget to (or get it wrong).
    /// `checksum`/`corrupt_for_test` on the struct itself stay public so
    /// tests can construct a record with a deliberately wrong checksum
    /// to exercise corruption detection.
    pub fn new(inode: InodeId, chunk_seq: u64, payload: Vec<u8>) -> Self {
        let checksum = crate::crc32c::checksum(&payload);
        Record {
            inode,
            chunk_seq,
            checksum,
            payload,
        }
    }
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

    /// Resumes writing after a recovery scan (`scan`'s `resume_at`)
    /// instead of starting from an empty segment — the normal path after
    /// a crash: replay/scan first, then keep appending from where the
    /// last *valid* record left off.
    pub fn resume(disk: D, base_offset: u64, resume_at: u64) -> Self {
        SegmentWriter {
            disk,
            base_offset,
            cursor: resume_at,
        }
    }

    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    pub fn remaining(&self) -> u64 {
        SEGMENT_SIZE_BYTES.saturating_sub(self.cursor)
    }

    /// Appends `record`, returning the offset it was written at (used to
    /// build the `ChunkPointer` stored in metadata). Fails if the record
    /// doesn't fit in the remaining space of this segment; the caller
    /// rolls over to a fresh segment in that case.
    pub fn append(&mut self, record: &Record) -> io::Result<u64> {
        let encoded = encode(record);
        if encoded.len() as u64 > self.remaining() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "segment full"));
        }
        let offset = self.base_offset + self.cursor;
        self.disk.write_at(offset, &encoded)?;
        self.cursor += encoded.len() as u64;
        Ok(offset)
    }

    pub fn sync(&self) -> io::Result<()> {
        self.disk.sync()
    }
}

fn encode(record: &Record) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + record.payload.len());
    buf.extend_from_slice(&RECORD_MAGIC.to_le_bytes());
    buf.extend_from_slice(&record.inode.to_le_bytes());
    buf.extend_from_slice(&record.chunk_seq.to_le_bytes());
    buf.extend_from_slice(&record.checksum.to_le_bytes());
    buf.extend_from_slice(&(record.payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&record.payload);
    buf
}

/// Result of scanning a segment from its start (IMPLEMENTATION_PLAN.md
/// P1.2 — the recovery path DESIGN.md §12's "power loss mid-write" row
/// promises).
#[derive(Debug)]
pub struct ScanResult {
    pub records: Vec<Record>,
    /// Offset (relative to `base_offset`) a `SegmentWriter` should
    /// resume appending at: right after the last valid record.
    pub resume_at: u64,
    /// Set if scanning stopped because a *fully present* record's
    /// checksum didn't match its payload — genuine corruption, distinct
    /// from simply running out of written data (no magic match) or an
    /// incomplete trailing record from a crash mid-write (short read).
    /// Neither of the latter two is reported as an error: they're the
    /// expected shape of "the writer stopped here," not a fault.
    pub corruption_at: Option<u64>,
}

/// Scans a segment from `base_offset`, decoding records until it hits
/// either the end of written data, an incomplete trailing record, or a
/// checksum mismatch. Never returns an `Err` for those cases — "how far
/// did the previous writer get" is exactly what this function exists to
/// answer, not something for it to fail on. Only real I/O errors (e.g.
/// the underlying disk is gone) propagate as `Err`.
pub fn scan<D: crate::disk::Disk>(disk: &D, base_offset: u64) -> io::Result<ScanResult> {
    let mut cursor = 0u64;
    let mut records = Vec::new();
    let mut corruption_at = None;

    loop {
        if cursor + HEADER_LEN as u64 > SEGMENT_SIZE_BYTES {
            break;
        }
        let mut header = [0u8; HEADER_LEN];
        if disk.read_at(base_offset + cursor, &mut header).is_err() {
            break; // ran out of backing storage — nothing more to scan
        }
        if header[0..4] != RECORD_MAGIC.to_le_bytes() {
            break; // unwritten (zero-filled) space: this is the resume point
        }
        let inode = u64::from_le_bytes(header[4..12].try_into().unwrap());
        let chunk_seq = u64::from_le_bytes(header[12..20].try_into().unwrap());
        let checksum = u32::from_le_bytes(header[20..24].try_into().unwrap());
        let len = u32::from_le_bytes(header[24..28].try_into().unwrap()) as u64;

        if cursor + HEADER_LEN as u64 + len > SEGMENT_SIZE_BYTES {
            break; // an impossible declared length is itself corruption of the header; treat like "incomplete"
        }
        let mut payload = vec![0u8; len as usize];
        if disk
            .read_at(base_offset + cursor + HEADER_LEN as u64, &mut payload)
            .is_err()
        {
            break; // incomplete trailing record — the write that would have completed it never landed
        }

        if crate::crc32c::checksum(&payload) != checksum {
            corruption_at = Some(cursor);
            break;
        }

        records.push(Record {
            inode,
            chunk_seq,
            checksum,
            payload,
        });
        cursor += HEADER_LEN as u64 + len;
    }

    Ok(ScanResult {
        records,
        resume_at: cursor,
        corruption_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::FileDisk;

    fn tempfile_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "tartine-segment-test-{}-{}",
            std::process::id(),
            name
        ));
        p
    }

    fn disk_of(name: &str, capacity: u64) -> (std::path::PathBuf, FileDisk) {
        let path = tempfile_path(name);
        let _ = std::fs::remove_file(&path);
        let disk = FileDisk::create(&path, capacity).unwrap();
        (path, disk)
    }

    #[test]
    fn round_trips_multiple_records() {
        let (path, disk) = disk_of("roundtrip", 4096);
        let mut w = SegmentWriter::new(disk, 0);
        w.append(&Record::new(1, 0, b"first".to_vec())).unwrap();
        w.append(&Record::new(1, 1, b"second".to_vec())).unwrap();
        w.append(&Record::new(2, 0, b"".to_vec())).unwrap(); // empty payload, exercised deliberately
        w.sync().unwrap();

        let reader = FileDisk::open(&path).unwrap();
        let result = scan(&reader, 0).unwrap();
        assert_eq!(result.records.len(), 3);
        assert_eq!(result.records[0].payload, b"first");
        assert_eq!(result.records[1].payload, b"second");
        assert_eq!(result.records[2].payload, b"" as &[u8]);
        assert!(result.corruption_at.is_none());
        assert_eq!(result.resume_at, w.cursor());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn recovers_from_truncated_trailing_record() {
        let (path, disk) = disk_of("truncated", 4096);
        let mut w = SegmentWriter::new(disk, 0);
        w.append(&Record::new(1, 0, b"complete".to_vec())).unwrap();
        let good_cursor = w.cursor();
        w.append(&Record::new(1, 1, b"never finishes".to_vec()))
            .unwrap();
        w.sync().unwrap();

        // Simulate a crash mid-write: truncate the backing file partway
        // through the second record's payload, as if the last write()
        // to the disk never completed.
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(good_cursor + HEADER_LEN as u64 + 3).unwrap();
        drop(f);

        let reader = FileDisk::open(&path).unwrap();
        let result = scan(&reader, 0).unwrap();
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].payload, b"complete");
        assert!(result.corruption_at.is_none());
        assert_eq!(result.resume_at, good_cursor);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn detects_corruption_in_a_fully_present_record() {
        let (path, disk) = disk_of("corrupt", 4096);
        let mut w = SegmentWriter::new(disk, 0);
        w.append(&Record::new(1, 0, b"good".to_vec())).unwrap();
        let mut bad = Record::new(1, 1, b"bitrotted".to_vec());
        bad.checksum ^= 0xffff_ffff; // deliberately wrong
        w.append(&bad).unwrap();
        w.sync().unwrap();

        let reader = FileDisk::open(&path).unwrap();
        let result = scan(&reader, 0).unwrap();
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].payload, b"good");
        assert!(result.corruption_at.is_some());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn scan_of_untouched_disk_finds_nothing() {
        let (path, disk) = disk_of("empty", 4096);
        let result = scan(&disk, 0).unwrap();
        assert!(result.records.is_empty());
        assert_eq!(result.resume_at, 0);
        assert!(result.corruption_at.is_none());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn writer_can_resume_after_recovery() {
        let (path, disk) = disk_of("resume", 4096);
        let mut w = SegmentWriter::new(disk, 0);
        w.append(&Record::new(1, 0, b"before crash".to_vec()))
            .unwrap();
        w.sync().unwrap();
        drop(w);

        let reader = FileDisk::open(&path).unwrap();
        let result = scan(&reader, 0).unwrap();
        drop(reader);

        let disk2 = FileDisk::open(&path).unwrap();
        let mut w2 = SegmentWriter::resume(disk2, 0, result.resume_at);
        w2.append(&Record::new(1, 1, b"after recovery".to_vec()))
            .unwrap();
        w2.sync().unwrap();

        let reader2 = FileDisk::open(&path).unwrap();
        let result2 = scan(&reader2, 0).unwrap();
        assert_eq!(result2.records.len(), 2);
        assert_eq!(result2.records[1].payload, b"after recovery");

        std::fs::remove_file(&path).unwrap();
    }
}
