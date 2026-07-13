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

impl TartineState {
    /// Hand-encoded rather than an `unsafe` struct-to-bytes cast —
    /// matches the byte-level encoding style the rest of this workspace
    /// uses (`tartine-meta::codec`, `tartine-core::segment`) rather than
    /// relying on `#[repr(C)]` layout matching via a raw memory copy.
    pub fn to_bytes(self) -> [u8; 24] {
        let mut buf = [0u8; 24];
        buf[0..4].copy_from_slice(&self.mode.to_le_bytes());
        buf[8..16].copy_from_slice(&self.bytes_total.to_le_bytes());
        buf[16..24].copy_from_slice(&self.bytes_converted.to_le_bytes());
        buf
    }
}

// ---------------------------------------------------------------------
// Redundancy policy get/set (DESIGN.md §10.3) — mirrors
// kernel/tartine.h's struct tartine_set_redundancy /
// struct tartine_replica_slot_spec byte-for-byte, so the exact same
// ioctl bytes work whether the mount is this FUSE prototype or the
// kernel module. `tartine_kcore::placement::ReplicaSlotSpec` already
// *is* the Rust type for one slot (24 bytes, matches the C struct) —
// reused directly rather than redefined here.
// ---------------------------------------------------------------------

use tartine_kcore::placement::{CLASS_ANY, CLASS_HDD, CLASS_NVME, CLASS_SSD, MAX_SELECT};
use tartine_proto::{DiskClass, RedundancyScheme, ReplicaSlot};

pub const TARTINE_REDUNDANCY_REPLICATED: u32 = 0;
/// Reserved, not implemented — DESIGN.md §16.6.
pub const TARTINE_REDUNDANCY_ERASURE_CODED: u32 = 1;

/// `16 (scheme_kind + n_slots + data_shards + parity_shards + _pad) +
/// MAX_SELECT * 24 (one ReplicaSlotSpec each)` = 400 bytes, matching
/// `kernel/tartine.h`'s `static_assert(sizeof(struct
/// tartine_set_redundancy) == 16 + TARTINE_MAX_REDUNDANCY_SLOTS * 24)`.
pub const REDUNDANCY_WIRE_SIZE: usize = 16 + MAX_SELECT * 24;

/// `_IOW('T', 3, tartine_set_redundancy)`.
pub const TARTINE_IOC_SET_REDUNDANCY: u32 = 0x4190_5403;
/// `_IOR('T', 4, tartine_set_redundancy)`.
pub const TARTINE_IOC_GET_REDUNDANCY: u32 = 0x8190_5404;

fn disk_class_to_wire(c: DiskClass) -> u8 {
    c.to_ffi()
}
fn disk_class_from_wire(v: u8) -> Option<DiskClass> {
    match v {
        CLASS_HDD => Some(DiskClass::Hdd),
        CLASS_SSD => Some(DiskClass::Ssd),
        CLASS_NVME => Some(DiskClass::Nvme),
        _ => None,
    }
}

/// Encodes a `RedundancyScheme` into the fixed `REDUNDANCY_WIRE_SIZE`
/// buffer the ioctl expects — the C struct's size is baked into the
/// `_IOW`/`_IOR` command number, so this is never a variable length.
pub fn encode_set_redundancy(scheme: &RedundancyScheme) -> [u8; REDUNDANCY_WIRE_SIZE] {
    let mut buf = [0u8; REDUNDANCY_WIRE_SIZE];
    match scheme {
        RedundancyScheme::Replicated(slots) => {
            buf[0..4].copy_from_slice(&TARTINE_REDUNDANCY_REPLICATED.to_le_bytes());
            buf[4..8].copy_from_slice(&(slots.len().min(MAX_SELECT) as u32).to_le_bytes());
            for (i, slot) in slots.iter().enumerate().take(MAX_SELECT) {
                let base = 16 + i * 24;
                let (required_class, pinned, hi, lo) = match slot {
                    ReplicaSlot::AnyOfClass(None) => (CLASS_ANY, 0u8, 0u64, 0u64),
                    ReplicaSlot::AnyOfClass(Some(c)) => (disk_class_to_wire(*c), 0u8, 0u64, 0u64),
                    ReplicaSlot::Pinned(id) => {
                        let (hi, lo) = tartine_core::placement::disk_id_to_hi_lo(*id);
                        (CLASS_ANY, 1u8, hi, lo)
                    }
                };
                buf[base] = required_class;
                buf[base + 1] = pinned;
                buf[base + 8..base + 16].copy_from_slice(&hi.to_le_bytes());
                buf[base + 16..base + 24].copy_from_slice(&lo.to_le_bytes());
            }
        }
        RedundancyScheme::ErasureCoded {
            data_shards,
            parity_shards,
        } => {
            buf[0..4].copy_from_slice(&TARTINE_REDUNDANCY_ERASURE_CODED.to_le_bytes());
            buf[8] = *data_shards;
            buf[9] = *parity_shards;
        }
    }
    buf
}

/// Decodes an ioctl-sized buffer back into a `RedundancyScheme`. Returns
/// `None` for a malformed buffer (wrong length, bad tag, `n_slots` over
/// `MAX_SELECT`, or an `AnyOfClass` slot naming an unrecognized class) —
/// callers map that to `-EINVAL`.
pub fn decode_set_redundancy(buf: &[u8]) -> Option<RedundancyScheme> {
    if buf.len() != REDUNDANCY_WIRE_SIZE {
        return None;
    }
    let scheme_kind = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    match scheme_kind {
        TARTINE_REDUNDANCY_REPLICATED => {
            let n_slots = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
            if n_slots > MAX_SELECT {
                return None;
            }
            let mut slots = Vec::with_capacity(n_slots);
            for i in 0..n_slots {
                let base = 16 + i * 24;
                let required_class = buf[base];
                let pinned = buf[base + 1];
                let hi = u64::from_le_bytes(buf[base + 8..base + 16].try_into().unwrap());
                let lo = u64::from_le_bytes(buf[base + 16..base + 24].try_into().unwrap());
                let slot = if pinned != 0 {
                    ReplicaSlot::Pinned(tartine_core::placement::disk_id_from_hi_lo(hi, lo))
                } else if required_class == CLASS_ANY {
                    ReplicaSlot::AnyOfClass(None)
                } else {
                    ReplicaSlot::AnyOfClass(Some(disk_class_from_wire(required_class)?))
                };
                slots.push(slot);
            }
            Some(RedundancyScheme::Replicated(slots))
        }
        TARTINE_REDUNDANCY_ERASURE_CODED => Some(RedundancyScheme::ErasureCoded {
            data_shards: buf[8],
            parity_shards: buf[9],
        }),
        _ => None,
    }
}

#[cfg(test)]
mod redundancy_wire_tests {
    use super::*;

    #[test]
    fn round_trips_any_and_class_and_pinned_slots() {
        let scheme = RedundancyScheme::Replicated(vec![
            ReplicaSlot::AnyOfClass(None),
            ReplicaSlot::AnyOfClass(Some(DiskClass::Ssd)),
            ReplicaSlot::Pinned(tartine_proto::Uuid(0x1234_5678_9abc_def0)),
        ]);
        let wire = encode_set_redundancy(&scheme);
        assert_eq!(wire.len(), REDUNDANCY_WIRE_SIZE);
        let decoded = decode_set_redundancy(&wire).unwrap();
        assert_eq!(decoded, scheme);
    }

    #[test]
    fn round_trips_erasure_coded_reservation() {
        let scheme = RedundancyScheme::ErasureCoded {
            data_shards: 4,
            parity_shards: 2,
        };
        let wire = encode_set_redundancy(&scheme);
        assert_eq!(decode_set_redundancy(&wire).unwrap(), scheme);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(decode_set_redundancy(&[0u8; 10]).is_none());
    }
}
