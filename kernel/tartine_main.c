// SPDX-License-Identifier: Apache-2.0
/*
 * tartine: pooled, replicated filesystem. See /DESIGN.md at the repo
 * root for the full design; this file is the VFS glue + control device
 * described in DESIGN.md's kernel/ section.
 *
 * Scope of this skeleton, explicitly: registration, mounting one pool
 * disk and reading its superblock, a minimal root directory, and the
 * ioctl surface (DESIGN.md §9.2) wired to tartine-kcore's write-path
 * state machine. Actual chunk-log/extent block I/O (DESIGN.md §5.3,
 * §11), the on-disk metadata B-tree and its 2-disk WAL replication
 * (DESIGN.md §6), and the rebalancer/scrubber (DESIGN.md §7, §8) are
 * NOT implemented here — those are large enough that hand-writing them
 * without a kernel build tree to compile/test against (unavailable in
 * the sandbox this was authored in) would produce code with unknown
 * correctness, which is worse than being explicit about the gap. Every
 * such gap is marked TODO with the relevant DESIGN.md section.
 *
 * Kernel API note: this targets a recent 6.x tree (written against
 * 6.18 conventions). The block-device-open APIs used by
 * tartine_ctl_scan_device() in particular have changed across recent
 * releases (blkdev_get_by_path -> bdev_open_by_path -> the
 * bdev_file_open_by_path()/`struct file *` form used below) — that is
 * the single most likely spot to need adjusting for whatever exact
 * kernel this is eventually built against.
 */

#include <linux/module.h>
#include <linux/fs.h>
#include <linux/fs_context.h>
#include <linux/buffer_head.h>
#include <linux/blkdev.h>
#include <linux/miscdevice.h>
#include <linux/uaccess.h>
#include <linux/slab.h>
#include <linux/crc32.h>
#include <linux/uuid.h>
#include <linux/mount.h>

#include "tartine.h"
#include "tartine_kcore.h"

MODULE_LICENSE("Dual BSD/GPL");
MODULE_DESCRIPTION("TartineFS: pooled, replicated filesystem (design skeleton)");
MODULE_AUTHOR("TartineFS");

static struct kmem_cache *tartine_inode_cache;

/* ---------------------------------------------------------------------
 * inode allocation
 * ------------------------------------------------------------------- */

static struct inode *tartine_alloc_inode(struct super_block *sb)
{
	struct tartine_inode_info *ti;

	ti = alloc_inode_sb(sb, tartine_inode_cache, GFP_KERNEL);
	if (!ti)
		return NULL;
	ti->mode = TARTINE_MODE_APPEND_ONLY;
	return &ti->vfs_inode;
}

static void tartine_free_inode(struct inode *inode)
{
	kmem_cache_free(tartine_inode_cache, TARTINE_I(inode));
}

static void tartine_inode_init_once(void *foo)
{
	struct tartine_inode_info *ti = foo;

	inode_init_once(&ti->vfs_inode);
}

/* ---------------------------------------------------------------------
 * ioctl (DESIGN.md §9.2): the one piece of the write path this skeleton
 * wires all the way through, since it's the part `tartine-kcore` already
 * implements and tests (crates/tartine-kcore/src/write_path.rs).
 * ------------------------------------------------------------------- */

static long tartine_file_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	struct inode *inode = file_inode(file);
	struct tartine_inode_info *ti = TARTINE_I(inode);

	switch (cmd) {
	case TARTINE_IOC_MAKE_WRITABLE: {
		__u32 flags;
		int new_mode;

		if (copy_from_user(&flags, (void __user *)arg, sizeof(flags)))
			return -EFAULT;

		new_mode = tartine_begin_convert(ti->mode);
		if (new_mode < 0) {
			switch (new_mode) {
			case TARTINE_ERR_ALREADY_WRITABLE:
			case TARTINE_ERR_ALREADY_CONVERTING:
				return -EALREADY;
			default:
				return -EINVAL;
			}
		}
		ti->mode = (__u32)new_mode;

		/*
		 * TODO(DESIGN.md §9.3 steps 2-5): kick off the background
		 * materializer (allocate the extent map, stream-copy the
		 * chunk-log into it, then call tartine_complete_convert()
		 * and swap ti->mode under the inode lock). Not implemented
		 * here; the mode transition above is real, the actual data
		 * migration is the missing piece.
		 */
		if (flags & TARTINE_CONVERT_FLAG_WAIT) {
			pr_warn("tartine: synchronous convert requested but materializer is not implemented yet\n");
			return -EOPNOTSUPP;
		}
		return 0;
	}
	case TARTINE_IOC_GET_STATE: {
		struct tartine_state state = {
			.mode = ti->mode,
			.bytes_total = i_size_read(inode),
			/* No materializer progress tracking yet (see TODO
			 * above), so a Converting file always reports 0/total
			 * rather than real progress. */
			.bytes_converted = ti->mode == TARTINE_MODE_CONVERTING ? 0 : i_size_read(inode),
		};

		if (copy_to_user((void __user *)arg, &state, sizeof(state)))
			return -EFAULT;
		return 0;
	}
	default:
		return -ENOTTY;
	}
}

static ssize_t tartine_file_write_iter(struct kiocb *iocb, struct iov_iter *from)
{
	struct inode *inode = file_inode(iocb->ki_filp);
	struct tartine_inode_info *ti = TARTINE_I(inode);
	loff_t offset = iocb->ki_pos;
	int kind;

	kind = tartine_classify_write(ti->mode, i_size_read(inode), (__u64)offset);
	switch (kind) {
	case TARTINE_ERR_NOT_APPEND_ORDER:
		return -EPERM;
	case TARTINE_ERR_CONVERSION_IN_PROGRESS:
		return -EAGAIN;
	case TARTINE_WRITE_KIND_APPEND:
	case TARTINE_WRITE_KIND_RANDOM:
		/*
		 * TODO(DESIGN.md §9.1, §11): the classification above is
		 * real and tested (tartine-kcore); the actual write —
		 * picking replicas via tartine_hrw_select(), submitting
		 * bios to them, and committing the metadata update — is
		 * not implemented. This is the module's single biggest
		 * gap, deliberately last on the roadmap (DESIGN.md §15)
		 * since every other piece needs to be solid first.
		 */
		return -EOPNOTSUPP;
	default:
		return -EINVAL;
	}
}

static int tartine_file_setattr(struct mnt_idmap *idmap, struct dentry *dentry, struct iattr *attr)
{
	struct inode *inode = d_inode(dentry);
	struct tartine_inode_info *ti = TARTINE_I(inode);

	if (attr->ia_valid & ATTR_SIZE) {
		int allowed = tartine_truncate_allowed(ti->mode);

		if (allowed != 0)
			return -EPERM;
	}
	/* TODO: the rest of setattr (uid/gid/mode/times) against the
	 * metadata store; not implemented (DESIGN.md §6 covers where this
	 * would eventually persist to). */
	return -EOPNOTSUPP;
}

static const struct file_operations tartine_file_fops = {
	.owner = THIS_MODULE,
	.write_iter = tartine_file_write_iter,
	.unlocked_ioctl = tartine_file_ioctl,
	.llseek = generic_file_llseek,
	/* TODO: .read_iter (DESIGN.md §11's read path), .mmap once
	 * Writable files exist. */
};

static const struct inode_operations tartine_file_iops = {
	.setattr = tartine_file_setattr,
};

/* ---------------------------------------------------------------------
 * directories: enough to have a browsable (empty) root, nothing else.
 * TODO(DESIGN.md §5.4): lookup/create/unlink/readdir all need the
 * metadata store's directory tree, which doesn't exist in this
 * skeleton yet.
 * ------------------------------------------------------------------- */

static const struct file_operations tartine_dir_fops = {
	.owner = THIS_MODULE,
	.iterate_shared = NULL, /* TODO: readdir over the directory tree */
	.llseek = generic_file_llseek,
};

static const struct inode_operations tartine_dir_iops = {
	/* TODO: .lookup, .create, .mkdir, .unlink, ... */
};

static struct inode *tartine_iget(struct super_block *sb, unsigned long ino)
{
	struct inode *inode;
	struct tartine_inode_info *ti;

	inode = iget_locked(sb, ino);
	if (!inode)
		return ERR_PTR(-ENOMEM);
	if (!(inode->i_state & I_NEW))
		return inode;

	ti = TARTINE_I(inode);

	if (ino == TARTINE_ROOT_INO) {
		inode->i_mode = S_IFDIR | 0755;
		inode->i_op = &tartine_dir_iops;
		inode->i_fop = &tartine_dir_fops;
		set_nlink(inode, 2);
		ti->mode = TARTINE_MODE_WRITABLE; /* directories aren't append-only */
	} else {
		inode->i_mode = S_IFREG | 0644;
		inode->i_op = &tartine_file_iops;
		inode->i_fop = &tartine_file_fops;
		set_nlink(inode, 1);
		ti->mode = TARTINE_MODE_APPEND_ONLY; /* DESIGN.md §9.1: born append-only */
	}
	inode->i_uid = GLOBAL_ROOT_UID;
	inode->i_gid = GLOBAL_ROOT_GID;
	simple_inode_init_ts(inode);

	unlock_new_inode(inode);
	return inode;
}

/* ---------------------------------------------------------------------
 * superblock
 * ------------------------------------------------------------------- */

static void tartine_put_super(struct super_block *sb)
{
	struct tartine_sb_info *sbi = sb->s_fs_info;

	/* TODO(DESIGN.md §6): flush/checkpoint the metadata WAL before
	 * releasing — nothing to flush yet since there's no metadata
	 * store in this skeleton. */
	kfree(sbi);
	sb->s_fs_info = NULL;
}

static int tartine_statfs(struct dentry *dentry, struct kstatfs *buf)
{
	/* TODO(DESIGN.md §7.1): sum capacity/used across every disk in the
	 * pool map, not just the one this superblock was mounted from. */
	buf->f_type = TARTINE_SB_MAGIC_U32;
	buf->f_bsize = TARTINE_SB_BLOCK_SIZE;
	buf->f_namelen = 255;
	return 0;
}

static const struct super_operations tartine_super_ops = {
	.alloc_inode = tartine_alloc_inode,
	.free_inode = tartine_free_inode,
	.put_super = tartine_put_super,
	.statfs = tartine_statfs,
};

static u32 tartine_super_checksum(const struct tartine_disk_super *ds)
{
	/* crc32c over every byte preceding the checksum field itself,
	 * matching DESIGN.md §5.2. Uses the kernel's own crc32c() (already
	 * hardware-accelerated where available) rather than anything from
	 * tartine-kcore — see that crate's doc comment for why checksums
	 * are deliberately left to the C side. */
	return crc32c(~0, ds, offsetof(struct tartine_disk_super, checksum));
}

static int tartine_fill_super(struct super_block *sb, struct fs_context *fc)
{
	struct buffer_head *bh;
	struct tartine_disk_super *ds;
	struct tartine_sb_info *sbi;
	struct inode *root_inode;
	int ret;

	sb_set_blocksize(sb, TARTINE_SB_BLOCK_SIZE);

	bh = sb_bread(sb, 0);
	if (!bh) {
		pr_err("tartine: unable to read superblock from %s\n", sb->s_id);
		return -EIO;
	}
	ds = (struct tartine_disk_super *)bh->b_data;

	if (memcmp(ds->magic, TARTINE_SB_MAGIC_STR, TARTINE_SB_MAGIC_LEN) != 0) {
		pr_err("tartine: %s is not a tartine disk (bad magic)\n", sb->s_id);
		brelse(bh);
		return -EINVAL;
	}
	if (le32_to_cpu(ds->checksum) != tartine_super_checksum(ds)) {
		pr_err("tartine: %s superblock checksum mismatch\n", sb->s_id);
		brelse(bh);
		return -EINVAL;
	}

	sbi = kzalloc(sizeof(*sbi), GFP_KERNEL);
	if (!sbi) {
		brelse(bh);
		return -ENOMEM;
	}
	import_uuid(&sbi->pool_id, ds->pool_id);
	import_uuid(&sbi->disk_id, ds->disk_id);
	sbi->format_version = le32_to_cpu(ds->format_version);
	sbi->roles = le32_to_cpu(ds->roles);
	brelse(bh);

	/*
	 * TODO(DESIGN.md §6, §7.1): this is where a real mount would union
	 * this disk's cached pool-map/meta-group with every other disk
	 * already registered via the control device (tartine_ctl_scan_device
	 * below), refuse to proceed if the metadata group can't be
	 * assembled, and otherwise bring up the rebalancer/scrubber. This
	 * skeleton mounts a single disk standalone.
	 */

	sb->s_magic = TARTINE_SB_MAGIC_U32;
	sb->s_op = &tartine_super_ops;
	sb->s_fs_info = sbi;
	sb->s_maxbytes = MAX_LFS_FILESIZE;
	sb->s_time_gran = 1;

	root_inode = tartine_iget(sb, TARTINE_ROOT_INO);
	if (IS_ERR(root_inode)) {
		ret = PTR_ERR(root_inode);
		goto out_free_sbi;
	}

	sb->s_root = d_make_root(root_inode);
	if (!sb->s_root) {
		ret = -ENOMEM;
		goto out_free_sbi;
	}
	return 0;

out_free_sbi:
	kfree(sbi);
	sb->s_fs_info = NULL;
	return ret;
}

/* ---------------------------------------------------------------------
 * fs_context / file_system_type (the modern, ≥5.1 mount API)
 * ------------------------------------------------------------------- */

static int tartine_get_tree(struct fs_context *fc)
{
	return get_tree_bdev(fc, tartine_fill_super);
}

static const struct fs_context_operations tartine_context_ops = {
	.get_tree = tartine_get_tree,
};

static int tartine_init_fs_context(struct fs_context *fc)
{
	fc->ops = &tartine_context_ops;
	return 0;
}

static void tartine_kill_sb(struct super_block *sb)
{
	kill_block_super(sb);
}

static struct file_system_type tartine_fs_type = {
	.owner = THIS_MODULE,
	.name = "tartine",
	.init_fs_context = tartine_init_fs_context,
	.kill_sb = tartine_kill_sb,
	.fs_flags = FS_REQUIRES_DEV,
};

/* ---------------------------------------------------------------------
 * control device: `/dev/tartine-ctl`, the btrfs-`device scan`-shaped
 * pre-mount device registration path (DESIGN.md kernel/ section,
 * tartine.h's comment on TARTINE_CTL_IOC_SCAN_DEVICE).
 * ------------------------------------------------------------------- */

static long tartine_ctl_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	struct tartine_ctl_scan_device req;

	switch (cmd) {
	case TARTINE_CTL_IOC_SCAN_DEVICE:
		if (copy_from_user(&req, (void __user *)arg, sizeof(req)))
			return -EFAULT;
		req.path[sizeof(req.path) - 1] = '\0';

		/*
		 * TODO(DESIGN.md kernel/ section): open req.path, read its
		 * superblock (same format tartine_fill_super() parses),
		 * and add it to an in-kernel registry keyed by pool_id so
		 * `mount -t tartine UUID=<pool-uuid> /mnt` can resolve to
		 * every currently-known member device, the way `btrfs
		 * device scan` populates btrfs's device registry. Not
		 * implemented: this ioctl currently only validates that
		 * the path was received, so `tartinectl` has something
		 * real to call while the registry itself is built out.
		 */
		pr_info("tartine: device scan requested for %s (registry not implemented yet)\n", req.path);
		return 0;
	default:
		return -ENOTTY;
	}
}

static const struct file_operations tartine_ctl_fops = {
	.owner = THIS_MODULE,
	.unlocked_ioctl = tartine_ctl_ioctl,
};

static struct miscdevice tartine_ctl_dev = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "tartine-ctl",
	.fops = &tartine_ctl_fops,
};

/* ---------------------------------------------------------------------
 * module init/exit
 * ------------------------------------------------------------------- */

static int __init tartine_init(void)
{
	int ret;

	tartine_inode_cache = kmem_cache_create(
		"tartine_inode_cache", sizeof(struct tartine_inode_info), 0,
		SLAB_RECLAIM_ACCOUNT | SLAB_ACCOUNT, tartine_inode_init_once);
	if (!tartine_inode_cache)
		return -ENOMEM;

	ret = misc_register(&tartine_ctl_dev);
	if (ret) {
		pr_err("tartine: failed to register control device: %d\n", ret);
		goto out_free_cache;
	}

	ret = register_filesystem(&tartine_fs_type);
	if (ret) {
		pr_err("tartine: failed to register filesystem: %d\n", ret);
		goto out_deregister_ctl;
	}

	pr_info("tartine: module loaded (design skeleton — see /DESIGN.md)\n");
	return 0;

out_deregister_ctl:
	misc_deregister(&tartine_ctl_dev);
out_free_cache:
	kmem_cache_destroy(tartine_inode_cache);
	return ret;
}

static void __exit tartine_exit(void)
{
	unregister_filesystem(&tartine_fs_type);
	misc_deregister(&tartine_ctl_dev);
	/* Make sure every inode's RCU-delayed free has actually happened
	 * before the slab cache they came from goes away. */
	rcu_barrier();
	kmem_cache_destroy(tartine_inode_cache);
}

module_init(tartine_init);
module_exit(tartine_exit);
