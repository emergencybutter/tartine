//! The append-only / converting / writable state machine (DESIGN.md §9,
//! §11). Primitive-typed (`mode`/`size`/`offset` as plain integers, not
//! `InodeRecord`) so it's directly usable across the C ABI: the kernel
//! module's inode struct carries the mode/size fields it already needs
//! for other reasons and just passes them through, no marshaling.
//!
//! This is the single implementation both front ends use: the FUSE
//! prototype (`tartine-fuse`) calls these functions as an ordinary Rust
//! dependency, and the kernel module calls them through the `#[no_mangle]`
//! wrappers — same compiled logic either way.

pub const MODE_APPEND_ONLY: u32 = 0;
pub const MODE_CONVERTING: u32 = 1;
pub const MODE_WRITABLE: u32 = 2;

pub const WRITE_KIND_APPEND: i32 = 0;
pub const WRITE_KIND_RANDOM: i32 = 1;

/// Negative return codes, chosen to read like kernel errno conventions
/// (a real C caller would map these to -EINVAL/-EPERM/-EAGAIN as
/// appropriate rather than surfacing the raw code to userspace).
pub const ERR_NOT_APPEND_ORDER: i32 = -1;
pub const ERR_RANDOM_WRITE_DENIED: i32 = -2;
pub const ERR_CONVERSION_IN_PROGRESS: i32 = -3;
pub const ERR_ALREADY_WRITABLE: i32 = -4;
pub const ERR_ALREADY_CONVERTING: i32 = -5;
pub const ERR_BAD_MODE: i32 = -6;

/// Classifies a `write(2)` at `offset` against a file of `size` bytes in
/// `mode`. Returns `WRITE_KIND_*` on success, `ERR_*` (negative) on
/// rejection.
#[no_mangle]
pub extern "C" fn tartine_classify_write(mode: u32, size: u64, offset: u64) -> i32 {
    match mode {
        MODE_APPEND_ONLY => {
            if offset == size {
                WRITE_KIND_APPEND
            } else {
                ERR_NOT_APPEND_ORDER
            }
        }
        MODE_CONVERTING => {
            // Appends past the point the materializer already captured
            // are still accepted (DESIGN.md §9.3 step 4); anything else
            // has to wait for the swap to `Writable`.
            if offset == size {
                WRITE_KIND_APPEND
            } else {
                ERR_CONVERSION_IN_PROGRESS
            }
        }
        MODE_WRITABLE => WRITE_KIND_RANDOM,
        _ => ERR_BAD_MODE,
    }
}

/// Whether `ftruncate`/similar is allowed in `mode`. Returns 0 (allowed)
/// or a negative `ERR_*`.
#[no_mangle]
pub extern "C" fn tartine_truncate_allowed(mode: u32) -> i32 {
    match mode {
        MODE_WRITABLE => 0,
        MODE_APPEND_ONLY | MODE_CONVERTING => ERR_RANDOM_WRITE_DENIED,
        _ => ERR_BAD_MODE,
    }
}

/// Step 1 of DESIGN.md §9.3: attempt `AppendOnly -> Converting`. Returns
/// the new mode (non-negative) on success, or a negative `ERR_*`.
#[no_mangle]
pub extern "C" fn tartine_begin_convert(mode: u32) -> i32 {
    match mode {
        MODE_APPEND_ONLY => MODE_CONVERTING as i32,
        MODE_WRITABLE => ERR_ALREADY_WRITABLE,
        MODE_CONVERTING => ERR_ALREADY_CONVERTING,
        _ => ERR_BAD_MODE,
    }
}

/// Step 5 of DESIGN.md §9.3: `Converting -> Writable`. The caller (kernel
/// module) is responsible for having already written and checksummed the
/// new extent map before calling this — by the time this runs there is
/// nothing left for it to fail on except a logic bug upstream, so, like
/// the original in-process version, it treats out-of-order calls as a
/// caller bug (`ERR_BAD_MODE`) rather than a recoverable condition.
#[no_mangle]
pub extern "C" fn tartine_complete_convert(mode: u32) -> i32 {
    if mode == MODE_CONVERTING {
        MODE_WRITABLE as i32
    } else {
        ERR_BAD_MODE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_only_accepts_write_at_eof() {
        assert_eq!(
            tartine_classify_write(MODE_APPEND_ONLY, 100, 100),
            WRITE_KIND_APPEND
        );
    }

    #[test]
    fn append_only_rejects_write_before_eof() {
        assert_eq!(
            tartine_classify_write(MODE_APPEND_ONLY, 100, 50),
            ERR_NOT_APPEND_ORDER
        );
    }

    #[test]
    fn append_only_rejects_truncate() {
        assert_eq!(
            tartine_truncate_allowed(MODE_APPEND_ONLY),
            ERR_RANDOM_WRITE_DENIED
        );
    }

    #[test]
    fn converting_still_accepts_tail_append_but_not_random_write() {
        assert_eq!(
            tartine_classify_write(MODE_CONVERTING, 100, 100),
            WRITE_KIND_APPEND
        );
        assert_eq!(
            tartine_classify_write(MODE_CONVERTING, 100, 10),
            ERR_CONVERSION_IN_PROGRESS
        );
    }

    #[test]
    fn convert_is_one_way() {
        let converting = tartine_begin_convert(MODE_APPEND_ONLY);
        assert_eq!(converting, MODE_CONVERTING as i32);

        let writable = tartine_complete_convert(converting as u32);
        assert_eq!(writable, MODE_WRITABLE as i32);
        assert_eq!(tartine_begin_convert(writable as u32), ERR_ALREADY_WRITABLE);

        assert_eq!(
            tartine_classify_write(writable as u32, 100, 0),
            WRITE_KIND_RANDOM
        );
        assert_eq!(tartine_truncate_allowed(writable as u32), 0);
    }
}
