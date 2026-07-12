/* SPDX-License-Identifier: Apache-2.0 */
/*
 * On-disk superblock layout (DESIGN.md §5.2), ioctl numbers (DESIGN.md
 * §9.2), and in-memory structs shared across the module's .c files.
 */
#ifndef TARTINE_H
#define TARTINE_H

#include <linux/types.h>
#include <linux/fs.h>
#include <linux/uuid.h>
#include <linux/build_bug.h>

#include "tartine_kcore.h" /* struct tartine_replica_slot_spec, TARTINE_CLASS_* */

#define TARTINE_SB_MAGIC_STR "TARTINE1"
#define TARTINE_SB_MAGIC_LEN 8
/* Arbitrary 32-bit value for sb->s_magic (procfs/statfs identify a
 * filesystem type by this, separately from the on-disk magic string). */
#define TARTINE_SB_MAGIC_U32 0x54415254u

#define TARTINE_SB_BLOCK_SIZE 4096
#define TARTINE_ROOT_INO 1

#define TARTINE_ROLE_DATA     (1u << 0)
#define TARTINE_ROLE_METADATA (1u << 1)

/* First 4 KiB of every pool disk. `__packed` and explicit-width fields
 * throughout because this crosses the disk boundary — no compiler is
 * allowed to insert padding or reinterpret field width here. */
struct tartine_disk_super {
	__u8  magic[TARTINE_SB_MAGIC_LEN];
	__u8  pool_id[16];
	__u8  disk_id[16];
	__le32 roles;
	__le32 format_version;
	__le64 created_at_unix;
	__le64 last_seen_epoch;
	__le32 checksum; /* crc32c of every preceding byte in this struct */
} __packed;

/* sb->s_fs_info. Deliberately minimal in this skeleton — a real build
 * additionally holds the in-memory pool map, the metadata group's
 * on-disk B-tree root, and the rebalancer/scrubber work queues here.
 * See DESIGN.md §5.1, §6, §7.1; none of that is implemented yet, only
 * structurally reserved. */
struct tartine_sb_info {
	uuid_t pool_id;
	uuid_t disk_id;
	__u32  format_version;
	__u32  roles;
};

/* Per-inode state kept in-core. `mode` uses the TARTINE_MODE_* constants
 * from tartine_kcore.h directly, so it can be passed to
 * tartine_classify_write()/friends with no translation.
 *
 * `redundancy_slots`/`n_redundancy_slots` are this file's placement
 * policy (DESIGN.md §10), stored in-core only for now — like `mode`
 * itself, there is no metadata store yet to persist it to (that's the
 * §6 B-tree, still TODO). New files start at n_redundancy_slots == 0
 * ("use the pool/directory default", also not wired up yet) rather than
 * a hardcoded default, so it's obvious when nothing has actually been
 * configured. */
struct tartine_inode_info {
	__u32 mode;
	__u32 n_redundancy_slots;
	struct tartine_replica_slot_spec redundancy_slots[TARTINE_MAX_REDUNDANCY_SLOTS];
	struct inode vfs_inode;
};

static inline struct tartine_inode_info *TARTINE_I(struct inode *inode)
{
	return container_of(inode, struct tartine_inode_info, vfs_inode);
}

/*
 * ioctl surface (DESIGN.md §9.2). Numbers match the ones already used by
 * the FUSE prototype's `tartine-fuse/src/ioctl.rs`, so `tartinectl` and
 * any script using `ioctl(2)`/`setfattr` directly see identical behavior
 * whether the mount is FUSE-backed (prototyping) or this kernel module
 * (production).
 */
#define TARTINE_IOC_MAGIC 'T'
#define TARTINE_IOC_MAKE_WRITABLE _IOW(TARTINE_IOC_MAGIC, 1, __u32)
#define TARTINE_IOC_GET_STATE     _IOR(TARTINE_IOC_MAGIC, 2, struct tartine_state)

#define TARTINE_CONVERT_FLAG_WAIT (1u << 0)

struct tartine_state {
	__u32 mode;
	/* Explicit padding: without it, 64-bit builds insert 4 invisible
	 * bytes here and 32-bit builds don't, so sizeof — and therefore the
	 * _IOR command number — would differ between 32- and 64-bit
	 * userspace. Always 0. Mirrored in tartine-fuse/src/ioctl.rs. */
	__u32 _pad;
	__u64 bytes_total;
	__u64 bytes_converted;
};

static_assert(sizeof(struct tartine_state) == 24);

/*
 * Per-file redundancy policy (DESIGN.md §10): "unreplicated on SSD",
 * "unreplicated pinned to a specific disk", "3x whichever disks", "one
 * HDD + one SSD so reads can hit the fast copy", and reserved syntax for
 * future erasure coding. `tartinectl` parses the human-friendly grammar
 * (crates/tartine-core/src/redundancy_spec.rs) into this *structured*
 * form client-side — the kernel never parses the string form for the
 * ioctl path, only (eventually) for the setxattr(2) convenience path,
 * which needs its own small parser since it can't call into userspace
 * Rust. `struct tartine_replica_slot_spec` (tartine_kcore.h) doubles as
 * both this ioctl's wire format and tartine_place_redundancy()'s input,
 * so no translation happens in between.
 */
#define TARTINE_MAX_REDUNDANCY_SLOTS TARTINE_MAX_SELECT

#define TARTINE_REDUNDANCY_REPLICATED     0u
/* Reserved, NOT IMPLEMENTED — DESIGN.md §16.6. TARTINE_IOC_SET_REDUNDANCY
 * rejects this with -EOPNOTSUPP; it exists in the wire format now so
 * accepting it later isn't a breaking ioctl-struct change. */
#define TARTINE_REDUNDANCY_ERASURE_CODED  1u

struct tartine_set_redundancy {
	__u32 scheme_kind; /* TARTINE_REDUNDANCY_* */
	__u32 n_slots;      /* valid when scheme_kind == REPLICATED */
	__u8  data_shards;   /* valid when scheme_kind == ERASURE_CODED (reserved) */
	__u8  parity_shards;  /* ditto */
	__u8  _pad[6];
	struct tartine_replica_slot_spec slots[TARTINE_MAX_REDUNDANCY_SLOTS];
};

static_assert(sizeof(struct tartine_set_redundancy) == 16 + TARTINE_MAX_REDUNDANCY_SLOTS * 24);

#define TARTINE_IOC_SET_REDUNDANCY _IOW(TARTINE_IOC_MAGIC, 3, struct tartine_set_redundancy)
#define TARTINE_IOC_GET_REDUNDANCY _IOR(TARTINE_IOC_MAGIC, 4, struct tartine_set_redundancy)

/*
 * Control device (`/dev/tartine-ctl`) ioctls, used before any pool disk
 * is mounted — registering member devices so `mount -t tartine
 * UUID=<pool-uuid> /mnt` can find all of them, the same role
 * `btrfs device scan` plays for btrfs multi-device pools (DESIGN.md's
 * kernel/ section). Administrative operations on an already-mounted
 * pool (disk add/remove, replication factor, meta disk pair) are a
 * separate ioctl set on the mountpoint itself, not modeled in this
 * skeleton yet.
 */
#define TARTINE_CTL_IOC_MAGIC 'C'
#define TARTINE_CTL_IOC_SCAN_DEVICE _IOW(TARTINE_CTL_IOC_MAGIC, 1, struct tartine_ctl_scan_device)

struct tartine_ctl_scan_device {
	char path[256]; /* e.g. "/dev/nvme3n1" */
};

#endif /* TARTINE_H */
