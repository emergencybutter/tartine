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
  tartined                FUSE prototype daemon (skeleton only)
  tartinectl              CLI — issues the same ioctls whether the mount
                          is the kernel module or the FUSE prototype
```

## Building

```sh
cargo build --workspace && cargo test --workspace   # Rust: core logic + FUSE prototype
cargo build --release -p tartine-kcore --features freestanding  # no_std smoke build
make -C kernel                                       # kernel module (unverified — see kernel/README.md)
```

The Rust workspace builds and tests clean with no external crates (see
the comment block in the root `Cargo.toml` for what a fuller prototype
build would add: `fuser`, `redb`, `tonic`, ...). `tartined`/`tartinectl`
in the FUSE prototype still just print what they'd do — there's no
working FUSE mount yet, only the tested logic underneath one.
