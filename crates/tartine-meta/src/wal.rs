//! Byte-level framing for the metadata WAL (DESIGN.md §6 steps 1-3):
//! each encoded `MetaOp` (`codec::encode_op`) is appended as one
//! magic+length+checksum-framed entry. Mirrors
//! `tartine_core::segment`'s framing pattern (a magic prefix so a reader
//! can tell a real entry from unwritten sparse space — see that module's
//! doc comment for why that distinction matters) but is its own,
//! separate implementation rather than sharing code with it: a segment
//! record's checksum protects a specific chunk's *data* for a different
//! consumer (read-time verification against what a chunk pointer
//! promises); a WAL frame's checksum protects an opaque encoded op
//! against WAL-file corruption. Forcing both onto one shared abstraction
//! would blur what each checksum is actually for.

use std::io;

use tartine_core::disk::Disk;

const WAL_MAGIC: u32 = 0x5741_4c31; // "WAL1"
const HEADER_LEN: usize = 4 + 4 + 4; // magic(4) len(4) checksum(4)

/// Sized generously for a prototype — this is a single mount's namespace
/// metadata, not the chunk-log data itself. A real implementation
/// checkpoints (DESIGN.md §6 step 4) well before this fills up;
/// `store.rs` doesn't checkpoint yet (IMPLEMENTATION_PLAN.md's P1.3
/// scope note), so for now this is simply "large enough for a prototype
/// session."
pub const WAL_CAPACITY_BYTES: u64 = 64 * 1024 * 1024;

fn frame(payload: &[u8]) -> Vec<u8> {
    let checksum = tartine_core::crc32c::checksum(payload);
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&WAL_MAGIC.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&checksum.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

#[derive(Debug)]
pub struct WalScanResult {
    pub entries: Vec<Vec<u8>>,
    /// Offset (relative to `base_offset`) the next `append` should write
    /// at — same "resume after recovery" role as
    /// `tartine_core::segment::ScanResult::resume_at`.
    pub resume_at: u64,
    pub corruption_at: Option<u64>,
}

/// Scans a WAL region from `base_offset`, decoding frames until it hits
/// the end of written data, an incomplete trailing frame (crash
/// mid-write), or a checksum mismatch (corruption) — same non-error
/// treatment of the first two cases as `segment::scan`, for the same
/// reason: "how far did the previous writer get" isn't a fault.
pub fn scan<D: Disk>(disk: &D, base_offset: u64) -> io::Result<WalScanResult> {
    let mut cursor = 0u64;
    let mut entries = Vec::new();
    let mut corruption_at = None;

    loop {
        if cursor + HEADER_LEN as u64 > WAL_CAPACITY_BYTES {
            break;
        }
        let mut header = [0u8; HEADER_LEN];
        if disk.read_at(base_offset + cursor, &mut header).is_err() {
            break;
        }
        if header[0..4] != WAL_MAGIC.to_le_bytes() {
            break;
        }
        let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64;
        let checksum = u32::from_le_bytes(header[8..12].try_into().unwrap());

        if cursor + HEADER_LEN as u64 + len > WAL_CAPACITY_BYTES {
            break;
        }
        let mut payload = vec![0u8; len as usize];
        if disk
            .read_at(base_offset + cursor + HEADER_LEN as u64, &mut payload)
            .is_err()
        {
            break;
        }
        if tartine_core::crc32c::checksum(&payload) != checksum {
            corruption_at = Some(cursor);
            break;
        }

        entries.push(payload);
        cursor += HEADER_LEN as u64 + len;
    }

    Ok(WalScanResult {
        entries,
        resume_at: cursor,
        corruption_at,
    })
}

/// Builds the bytes for one WAL entry and how much space it needs —
/// callers that must write the identical frame to two disks (`store.rs`,
/// via `MetaReplicator::commit`) build the frame once and pass the same
/// bytes to both, rather than each disk independently framing (and
/// potentially disagreeing on) the same payload.
pub fn build_frame(payload: &[u8]) -> Vec<u8> {
    frame(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tartine_core::disk::FileDisk;

    fn tempfile_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("tartine-wal-test-{}-{}", std::process::id(), name));
        p
    }

    #[test]
    fn round_trips_multiple_entries() {
        let path = tempfile_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let disk = FileDisk::create(&path, WAL_CAPACITY_BYTES).unwrap();

        let mut cursor = 0u64;
        for payload in [
            b"first entry".as_slice(),
            b"second".as_slice(),
            b"".as_slice(),
        ] {
            let f = build_frame(payload);
            disk.write_at(cursor, &f).unwrap();
            cursor += f.len() as u64;
        }
        disk.sync().unwrap();

        let result = scan(&disk, 0).unwrap();
        assert_eq!(
            result.entries,
            vec![b"first entry".to_vec(), b"second".to_vec(), b"".to_vec()]
        );
        assert_eq!(result.resume_at, cursor);
        assert!(result.corruption_at.is_none());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn recovers_from_truncated_trailing_entry() {
        let path = tempfile_path("truncated");
        let _ = std::fs::remove_file(&path);
        let disk = FileDisk::create(&path, WAL_CAPACITY_BYTES).unwrap();

        let f1 = build_frame(b"complete entry");
        disk.write_at(0, &f1).unwrap();
        let good_cursor = f1.len() as u64;
        let f2 = build_frame(b"this one gets cut off");
        disk.write_at(good_cursor, &f2).unwrap();
        disk.sync().unwrap();
        drop(disk);

        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(good_cursor + 5).unwrap();
        drop(f);

        let reader = FileDisk::open(&path).unwrap();
        let result = scan(&reader, 0).unwrap();
        assert_eq!(result.entries, vec![b"complete entry".to_vec()]);
        assert_eq!(result.resume_at, good_cursor);
        assert!(result.corruption_at.is_none());

        std::fs::remove_file(&path).unwrap();
    }
}
