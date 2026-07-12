# kernel/ — the tartine kernel module

The production target (see `/DESIGN.md`'s "kernel/" section): a real
`file_system_type` registered via `register_filesystem()`, mountable with
`mount -t tartine /dev/<disk> /mnt`, with no userspace daemon required to
operate — same operating model as ext4/btrfs/xfs. The FUSE-based prototype
(`crates/tartine-fuse`, `crates/tartined`) exists to validate the design
before committing it to kernel code, not as an alternative production
target.

## What's real here vs. what's a stub

- **Real, and cross-validated against tests**: the placement algorithm
  (HRW) and the append-only/converting/writable state machine, via
  `crates/tartine-kcore`, linked into this module as a staticlib
  (`tartine_kcore.h` is the hand-maintained header for its `extern "C"`
  surface). Every decision `tartine_main.c` makes about *whether* a write
  is allowed, or *which* disks a chunk should land on, calls into that
  crate — it is not reimplemented in C.
- **Real VFS/kbuild plumbing**: module init/exit, `file_system_type`
  registration via the modern `fs_context` mount API, superblock
  read+checksum+parse (`tartine_fill_super`), a minimal root inode, and
  the `ioctl(2)` surface for the append→writable conversion trigger
  (DESIGN.md §9.2).
- **Stubbed, explicitly, with `TODO(DESIGN.md §...)` comments at each
  site**: actual chunk-log/extent I/O against the block layer (every
  `write_iter`/`read_iter` call returns `-EOPNOTSUPP` right now), the
  on-disk metadata B-tree and its 2-disk WAL replication, the
  rebalancer/scrubber, and the device-scan registry the control device
  (`/dev/tartine-ctl`) is meant to populate. These are large enough that
  writing them by hand with no kernel build tree to compile/test against
  would produce code of unknown correctness — worse than being explicit
  about the gap.

## What has *not* been verified, and why

No kernel headers (`/lib/modules/$(uname -r)/build`) were available in
the sandbox this was written in — `apt`/`dnf` package managers weren't
usable to install `linux-headers-*`, and there's no kernel source tree
checked out anywhere accessible. That means:

- `tartine_main.c` has never been compiled. It's written against
  standard, long-stable VFS/kbuild conventions (the parts I'm most
  confident about: `fs_context`, `iget_locked`, `sb_bread`, `kmem_cache`
  patterns), but the block-device-open family of APIs in particular
  (`blkdev_get_by_path` → `bdev_open_by_path` → `bdev_file_open_by_path`)
  has churned release to release; whichever kernel this is actually
  built against may need that one adjusted. Treat this file as a strong
  first draft to compile against a real tree and fix forward from, not
  as verified code.
- `Makefile`'s approach to linking `tartine-kcore`'s prebuilt staticlib
  into the kbuild module link step (extracting the `.a`'s object members
  and listing them in `tartine-objs`) is a standard technique but hasn't
  been exercised here. The most likely first failure is a symbol
  collision between the `compiler_builtins` objects pulled in by
  `tartine-kcore`'s freestanding build and the kernel's own equivalents
  (things like `__udivdi3`) — if that happens, the fix is either
  `--allow-multiple-definition` at the final link (risky — verify the
  definitions actually agree) or configuring `compiler_builtins` to
  weaken/omit the colliding symbols.
- Nothing here has been through `kernel_get_fpu`/no-redzone/kernel
  code-model verification (see below) — the `freestanding` build in this
  repo only proves the *logic* compiles clean under `#![no_std]` for the
  host target, not that the resulting object is safe to link into a
  running kernel.

What **is** verified: `cargo build --release -p tartine-kcore --features
freestanding` (run from the repo root) compiles clean, and `nm` on the
resulting object confirms exactly the expected exported symbols
(`tartine_hrw_select`, `tartine_classify_write`, etc.) with no stray
`std`/libc references — see the crate's doc comment for exactly what that
does and doesn't prove.

## Making the Rust object actually kernel-safe

Compiling `tartine-kcore --features freestanding` for the default host
target (as this repo's `cargo build` does) produces an object built for a
*hosted* environment's ABI assumptions. Linking that directly into a
kernel would be unsafe in several concrete ways a real build needs to
close, all standard practice for freestanding kernel Rust (this is
exactly what Rust-for-Linux's build integration does, even though this
project isn't using the `kernel` crate itself):

- **Disable the x86-64 red zone** (`-C no-redzone`): the kernel doesn't
  reserve the 128-byte red zone below `%rsp` that the System V ABI
  assumes, so code compiled without this flag can silently corrupt
  interrupt-handler state.
- **Kernel code model** (`-C code-model=kernel`): matches how the kernel
  expects symbols to be addressed (negative 2 GiB range), consistent
  with the rest of a `vmlinux`/module build.
- **`panic = "abort"`, no unwinding**: already set (see the crate's own
  `#[panic_handler]` and this workspace's `[profile.release]`), since
  there's no unwind runtime under a kernel module.
- **No implicit FPU/SIMD use**: the crate already avoids floating point
  by design (fixed-point weights — see `tartine_disk_candidate`'s
  comment), but a real build should also pass codegen flags disabling
  SSE/AVX codegen for scalar operations, since the kernel doesn't save
  FPU state around arbitrary function calls without `kernel_fpu_begin()`.
- **`memcpy`/`memset`/`memmove`/`bcmp`**: Rust codegen still emits calls
  to these for slice/struct operations. There's no libc to provide them
  under a kernel module — the kernel already defines all four itself, so
  the linker resolves against the kernel's versions when this object is
  linked into the `.ko`; nothing extra to provide, just don't let the
  build accidentally pull in Rust's own `compiler_builtins` definitions
  of them instead (a real config disables `compiler-builtins-mem` /
  passes `-C link-dead-code=no` appropriately — exact flag needs
  checking against whatever `rustc`/`cargo` version is in use).

In practice, standing this up for real means either a custom target spec
JSON (as Rust-for-Linux generates via `scripts/generate_rust_target.rs`)
or `-Z build-std=core --target <arch>-unknown-none` plus the flags above,
selected and iterated against an actual kernel build tree. None of that
is wired up in this repo yet — it's the next concrete step once a real
build environment is available.

## Building (once the above is addressed)

```sh
make -C kernel                 # builds tartine-kcore, then the .ko
sudo insmod kernel/tartine.ko
ls /dev/tartine-ctl            # control device should now exist
```
