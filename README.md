# TartineFS

A Linux filesystem that pools together a dynamic set of local disks,
replicates file data per-file and metadata on a small reconfigurable set
of disks, and gives files append-only-by-default semantics with an
explicit, costly, one-way upgrade to fully random-access writable files.

**Production target is a real in-kernel filesystem** (`kernel/`, C,
`mount -t tartine`) — no userspace daemon required, same operating model
as ext4/btrfs/xfs. The Rust/FUSE code under `crates/` is a prototyping
step used to validate the design before committing it to kernel code, not
an alternative production target.

See **[DESIGN.md](DESIGN.md)** for the full architecture and rationale,
**[IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md)** for the ordered
milestone plan that turns it into working code, and
**[kernel/README.md](kernel/README.md)** for exactly what in the
kernel module is real versus stubbed today (short version: VFS
registration and the ioctl/write-classification surface are wired up and
call into tested Rust logic; block I/O, the on-disk metadata B-tree, and
the rebalancer/scrubber are not implemented yet, and none of the C has
been build-verified — no kernel headers were available in the sandbox
this was authored in).

## Repository layout

```
DESIGN.md            full architecture, rationale, open questions
kernel/               production target: the C kernel module
  tartine_main.c       VFS glue, superblock, ioctl handling
  tartine.h             on-disk superblock format, ioctl numbers
  tartine_kcore.h        hand-maintained header for tartine-kcore's FFI
  Makefile               kbuild + links tartine-kcore's staticlib in
  README.md               what's real, what's stubbed, what's unverified
crates/               Rust workspace: one shared core, one prototype
  tartine-kcore          #![no_std], zero deps: HRW placement + the
                          append-only/converting/writable state machine.
                          Compiled two ways — a staticlib linked into the
                          kernel module, and a plain rlib the FUSE
                          prototype calls directly — so both front ends
                          run the exact same compiled logic.
  tartine-proto           shared on-disk/wire types for the FUSE prototype
  tartine-core            FUSE prototype: chunk-log segment format, thin
                          wrapper around tartine-kcore's placement
  tartine-meta            FUSE prototype: metadata store + WAL replication
                          protocol (redb-backed; production uses its own
                          in-kernel B-tree instead — see DESIGN.md §4)
  tartine-fuse            FUSE prototype: write-path adapter over
                          tartine-kcore, the ioctl surface
  tartined                FUSE prototype daemon — assembles a Pool from
                          disk-backing files and mounts it via `fuser`
  tartinectl              CLI — issues real ioctl(2) calls, the same
                          wire format whether the mount is the kernel
                          module or the FUSE prototype
```

## Building

```sh
cargo build --workspace && cargo test --workspace   # Rust: core logic + FUSE prototype
cargo build --release -p tartine-kcore --features freestanding  # no_std smoke build
make -C kernel                                       # kernel module (unverified — see kernel/README.md)
```

`cargo test --workspace` runs 67 tests across `tartine-core`,
`tartine-meta`, `tartine-kcore`, and `tartine-fuse`, covering crash
recovery (truncated segment/WAL records), 2-disk metadata replication
with fault-injected backup failure, and full append/convert/random-write
round trips through `Pool`. The FUSE prototype is a real, mountable
filesystem:

```sh
tartined --disk a.img:hdd --disk b.img:hdd --disk c.img:ssd /mnt/t
```

mounts append-only-by-default files with per-file redundancy policy
(`tartinectl file set-redundancy <path> <spec>`), truncate rejection,
and the append→writable conversion ioctl
(`tartinectl convert <path> --wait`), all backed by real disk I/O and
2-disk metadata replication — see `IMPLEMENTATION_PLAN.md`'s §P1.5 for
the exact transcript this has been run against, live, in this repo's
sandbox. See `IMPLEMENTATION_PLAN.md`'s "Current status" section for
what's still a stub (rebalancer, disk add/remove, persisted superblock,
sub-block random writes) versus what's real.
