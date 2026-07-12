/* SPDX-License-Identifier: Apache-2.0 */
/*
 * Hand-maintained header for the FFI surface exported by
 * crates/tartine-kcore (built with `--features freestanding` as a
 * staticlib — see that crate's Cargo.toml and lib.rs doc comment, and
 * DESIGN.md's kernel/ section for how the two sides are kept in sync).
 *
 * There is no cbindgen step generating this automatically (no network
 * access to fetch it in the sandbox this was written in); every symbol
 * here must continue to match crates/tartine-kcore/src/{placement,write_path}.rs
 * by hand. The Rust side uses `#[no_mangle] extern "C"` specifically so
 * these names and signatures are exactly what a linker/header see, with
 * no name-mangling surprises to keep in sync separately.
 */
#ifndef TARTINE_KCORE_H
#define TARTINE_KCORE_H

#include <linux/types.h>
#include <linux/build_bug.h>

/* Mirrors tartine_kcore::placement::DiskCandidate (#[repr(C)]). Weight is
 * fixed-point (real_weight * 1000): kernel code must not touch the FPU
 * casually, so there is no f64 anywhere on this boundary. Selection
 * probability is exactly proportional to weight — the Rust side scores
 * with logarithmic weighted rendezvous (Thaler-Ravishankar), not the
 * biased hash-times-weight shortcut. */
struct tartine_disk_candidate {
	__u64 disk_id_hi;
	__u64 disk_id_lo;
	__u32 weight_milli;
	__u8  active;
};

/* Layout contract with the Rust side (which carries the matching
 * compile-time asserts): if either side drifts, the build fails instead
 * of the FFI silently misreading memory. */
static_assert(sizeof(struct tartine_disk_candidate) == 24);

#define TARTINE_MAX_SELECT 16u

/*
 * Selects up to n_out active candidates for `key`, highest score first,
 * writing their *index into candidates* into `out`. Returns the number
 * written (may be less than n_out if there aren't enough active
 * candidates — DESIGN.md §7.4 treats that as "under-replicated, repair
 * when possible", not an error).
 */
size_t tartine_hrw_select(const struct tartine_disk_candidate *candidates,
			   size_t n_candidates, __u64 key, __u32 *out,
			   size_t n_out);

/* Placement key for (inode, chunk_seq) / (inode, extent_index). */
__u64 tartine_hash_key(__u64 inode, __u64 seq);

/* Inode mode constants — mirror tartine_kcore::write_path::MODE_*. */
#define TARTINE_MODE_APPEND_ONLY 0u
#define TARTINE_MODE_CONVERTING  1u
#define TARTINE_MODE_WRITABLE    2u

/* Positive/zero returns from tartine_classify_write(). */
#define TARTINE_WRITE_KIND_APPEND 0
#define TARTINE_WRITE_KIND_RANDOM 1

/* Negative returns shared by the functions below — map to -EPERM/-EAGAIN/
 * -EINVAL at the VFS call site, not surfaced to userspace as-is. */
#define TARTINE_ERR_NOT_APPEND_ORDER     (-1)
#define TARTINE_ERR_RANDOM_WRITE_DENIED  (-2)
#define TARTINE_ERR_CONVERSION_IN_PROGRESS (-3)
#define TARTINE_ERR_ALREADY_WRITABLE     (-4)
#define TARTINE_ERR_ALREADY_CONVERTING   (-5)
#define TARTINE_ERR_BAD_MODE             (-6)

/* Classifies a write(2) at `offset` against a file of `size` bytes
 * currently in `mode`. Returns TARTINE_WRITE_KIND_* or a negative
 * TARTINE_ERR_*. See DESIGN.md §9, §11. */
int tartine_classify_write(__u32 mode, __u64 size, __u64 offset);

/* Whether ftruncate(2)/similar is allowed in `mode`. 0 = allowed. */
int tartine_truncate_allowed(__u32 mode);

/* AppendOnly -> Converting (DESIGN.md §9.3 step 1). Returns the new mode
 * (>= 0) or a negative TARTINE_ERR_*. */
int tartine_begin_convert(__u32 mode);

/* Converting -> Writable (DESIGN.md §9.3 step 5). Caller must have
 * already durably written and checksummed the new extent map. Returns
 * the new mode (>= 0) or TARTINE_ERR_BAD_MODE if called out of order. */
int tartine_complete_convert(__u32 mode);

#endif /* TARTINE_KCORE_H */
