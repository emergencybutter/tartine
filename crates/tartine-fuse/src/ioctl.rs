//! The `ioctl` surface described in DESIGN.md §9.2, mirrored here as a
//! Rust-side counterpart to `kernel/tartine.h` for C callers. Command
//! numbers follow the standard Linux `_IOW`/`_IOR` encoding (magic `'T'`).

/// `_IOW('T', 1, u32)` — trigger the append-only -> writable conversion.
pub const TARTINE_IOC_MAKE_WRITABLE: u32 = 0x4004_5401;
/// `_IOR('T', 2, tartine_state)` — poll conversion progress / current
/// mode. Size field is 24 = `size_of::<TartineState>()`, which must stay
/// identical for 32- and 64-bit userspace (hence the explicit `_pad`
/// below) or one of the two gets `-ENOTTY` from a size-checked ioctl
/// dispatch.
pub const TARTINE_IOC_GET_STATE: u32 = 0x8018_5402;

/// Bit 0 of the `flags` argument to `TARTINE_IOC_MAKE_WRITABLE`: if set,
/// the ioctl blocks until conversion is fully complete; if clear, it
/// returns immediately and the caller polls `TARTINE_IOC_GET_STATE`.
pub const TARTINE_CONVERT_FLAG_WAIT: u32 = 1 << 0;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TartineState {
    pub mode: u32,
    /// Explicit padding: without it, 64-bit compilers insert 4 invisible
    /// bytes before `bytes_total` and 32-bit compilers don't, so the
    /// struct (and therefore the `_IOR` command number, which encodes
    /// sizeof) would differ between 32- and 64-bit userspace. Always 0.
    pub _pad: u32,
    pub bytes_total: u64,
    pub bytes_converted: u64,
}

// Mirrors the static_assert in kernel/tartine.h — both sides refuse to
// compile if the ABI drifts.
const _: () = assert!(core::mem::size_of::<TartineState>() == 24);

pub const TARTINE_MODE_APPEND_ONLY: u32 = 0;
pub const TARTINE_MODE_CONVERTING: u32 = 1;
pub const TARTINE_MODE_WRITABLE: u32 = 2;
