//! `tartined` — the single daemon that owns a pool: mounts the namespace
//! over FUSE, holds the metadata replicator, runs the rebalancer/scrubber,
//! and serves the `tartinectl` control API. See DESIGN.md §3 for the
//! overall wiring this `main` would perform in a real build:
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
    eprintln!("tartined: design skeleton only, no pool implementation yet");
    eprintln!("see DESIGN.md for the architecture this binary will wire up");
    std::process::exit(1);
}
