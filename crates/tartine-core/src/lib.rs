//! Disk-facing primitives: raw disk I/O abstraction, the append-only
//! chunk-log segment format, and HRW placement. No metadata, no FUSE —
//! see `tartine-meta` and `tartine-fuse` for those layers. DESIGN.md is
//! the source of truth this crate implements slices of.

pub mod disk;
pub mod placement;
pub mod segment;
