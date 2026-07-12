//! The `ioctl` surface described in DESIGN.md §9.2, mirrored here as a
//! Rust-side counterpart to the `tartine_ioctl.h` header that would ship
//! for C callers. Command numbers follow the standard Linux `_IOW`/`_IOR`
//! encoding (magic `'T'`).

/// `_IOW('T', 1, u32)` — trigger the append-only -> writable conversion.
pub const TARTINE_IOC_MAKE_WRITABLE: u32 = 0x4004_5401;
/// `_IOR('T', 2, tartine_state)` — poll conversion progress / current mode.
pub const TARTINE_IOC_GET_STATE: u32 = 0x8010_5402;

/// Bit 0 of the `flags` argument to `TARTINE_IOC_MAKE_WRITABLE`: if set,
/// the ioctl blocks until conversion is fully complete; if clear, it
/// returns immediately and the caller polls `TARTINE_IOC_GET_STATE`.
pub const TARTINE_CONVERT_FLAG_WAIT: u32 = 1 << 0;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TartineState {
    pub mode: u32,
    pub bytes_total: u64,
    pub bytes_converted: u64,
}

pub const TARTINE_MODE_APPEND_ONLY: u32 = 0;
pub const TARTINE_MODE_CONVERTING: u32 = 1;
pub const TARTINE_MODE_WRITABLE: u32 = 2;
