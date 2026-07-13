//! Abstraction over a single pool disk's raw I/O.
//!
//! A real implementation backs this with `io_uring` + `O_DIRECT` against a
//! block device (see DESIGN.md §4); `FileDisk` below is what the FUSE
//! prototype actually uses (IMPLEMENTATION_PLAN.md P1.1) — plain
//! synchronous file I/O against a regular file standing in for a block
//! device. Kept as a trait so `tartine-meta`'s WAL shipping and
//! `tartine-core`'s segment log don't need to know which.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub trait Disk: Send + Sync {
    /// Read `buf.len()` bytes starting at `offset`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Write `buf` at `offset`. Does not imply durability — call `sync`.
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()>;

    /// Force previously issued writes to stable storage
    /// (`fdatasync`/`fsync` equivalent).
    fn sync(&self) -> io::Result<()>;

    fn capacity_bytes(&self) -> u64;
}

/// A pool "disk" backed by a plain regular file. Real block-device I/O
/// (`O_DIRECT`, `io_uring`) is a performance concern, not a correctness
/// one — this is deliberately the simplest thing that satisfies the
/// `Disk` contract, so the segment/metadata logic built on top of it can
/// be gotten right before anything async enters the picture
/// (IMPLEMENTATION_PLAN.md P1.1).
pub struct FileDisk {
    file: File,
    capacity_bytes: u64,
}

impl FileDisk {
    /// Creates a new backing file of exactly `capacity_bytes`, sized with
    /// `set_len` (sparse — the filesystem underneath `path` decides how
    /// much is actually allocated up front).
    pub fn create(path: &Path, capacity_bytes: u64) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.set_len(capacity_bytes)?;
        Ok(FileDisk {
            file,
            capacity_bytes,
        })
    }

    /// Opens an existing backing file; capacity is whatever the file's
    /// current length is.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let capacity_bytes = file.metadata()?.len();
        Ok(FileDisk {
            file,
            capacity_bytes,
        })
    }
}

#[cfg(unix)]
impl Disk for FileDisk {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, offset)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.write_all_at(buf, offset)
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }
}

/// Lets one physical disk be shared between multiple owners — e.g. a
/// `Pool` (IMPLEMENTATION_PLAN.md P1.4) handing the same disk to a
/// `SegmentWriter` for chunk-log data *and* to a `MetaStore` as a
/// metadata replica, when that disk holds both roles (DESIGN.md §5.1:
/// "a disk can hold both roles simultaneously"). Sound because every
/// `Disk` method already takes `&self`, matching how `pread`/`pwrite`
/// are safe to call concurrently from multiple call sites against the
/// same open file descriptor.
impl<T: Disk + ?Sized> Disk for std::sync::Arc<T> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        (**self).write_at(offset, buf)
    }
    fn sync(&self) -> io::Result<()> {
        (**self).sync()
    }
    fn capacity_bytes(&self) -> u64 {
        (**self).capacity_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn tempfile_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "tartine-filedisk-test-{}-{}",
            std::process::id(),
            name
        ));
        p
    }

    #[test]
    fn write_then_read_back() {
        let path = tempfile_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let disk = FileDisk::create(&path, 4096).unwrap();

        disk.write_at(100, b"hello disk").unwrap();
        disk.sync().unwrap();

        let mut buf = [0u8; 10];
        disk.read_at(100, &mut buf).unwrap();
        assert_eq!(&buf, b"hello disk");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn persists_across_reopen() {
        let path = tempfile_path("persist");
        let _ = std::fs::remove_file(&path);
        {
            let disk = FileDisk::create(&path, 4096).unwrap();
            disk.write_at(0, b"still here").unwrap();
            disk.sync().unwrap();
        }
        {
            let disk = FileDisk::open(&path).unwrap();
            assert_eq!(disk.capacity_bytes(), 4096);
            let mut buf = [0u8; 10];
            disk.read_at(0, &mut buf).unwrap();
            assert_eq!(&buf, b"still here");
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn read_past_end_of_file_errors() {
        let path = tempfile_path("short-read");
        let _ = std::fs::remove_file(&path);
        let disk = FileDisk::create(&path, 16).unwrap();
        let mut buf = [0u8; 32];
        let err = disk.read_at(0, &mut buf).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
        std::fs::remove_file(&path).unwrap();
    }
}
