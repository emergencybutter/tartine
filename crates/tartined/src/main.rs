//! `tartined` — the FUSE **prototype's** daemon: mounts the namespace
//! over FUSE, holds the metadata replicator, runs the rebalancer/scrubber,
//! and serves the `tartinectl` control API. This exists to validate the
//! design end-to-end in userspace before it's committed to kernel code
//! (`kernel/`, the actual production target — see DESIGN.md's opening
//! note and §3/§4). See DESIGN.md §3 for the overall wiring this `main`
//! would perform in a real prototype build:
//!
//!   1. Read every attached disk's superblock, union their cached
//!      `MetaGroup`/`PoolMap` echoes, pick the highest-epoch answer.
//!   2. Open the metadata store on the resulting primary (+ backup)
//!      disk(s), replay the WAL since the last checkpoint.
//!   3. Start the rebalancer, scrubber, and control-socket server as
//!      background tasks.
//!   4. Mount the FUSE session (`tartine_fuse::TartineFs`) and serve.
//!
//! Left as a stub: this binary's job is to prove the crates in this
//! workspace fit together, not to duplicate `DESIGN.md`.

fn main() {
    eprintln!("tartined: FUSE prototype skeleton only, no pool implementation yet");
    eprintln!("production target is the kernel module in kernel/ — see DESIGN.md");
    std::process::exit(1);
}
