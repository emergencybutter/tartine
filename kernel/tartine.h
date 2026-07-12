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
 * tartine_classify_write()/friends with no translation. */
struct tartine_inode_info {
	__u32 mode;
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
	__u64 bytes_total;
	__u64 bytes_converted;
};

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
