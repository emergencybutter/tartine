//! Abstraction over a single pool disk's raw I/O.
//!
//! A real implementation backs this with `io_uring` + `O_DIRECT` against a
//! block device (see DESIGN.md §4); tests and early prototyping can back
//! it with a plain file. Kept as a trait so `tartine-meta`'s WAL shipping
//! and `tartine-core`'s segment log don't need to know which.

use std::io;

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
