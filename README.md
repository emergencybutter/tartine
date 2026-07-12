# TartineFS

A Linux filesystem that pools together a dynamic set of local disks,
replicates file data per-file and metadata on a small reconfigurable set
of disks, and gives files append-only-by-default semantics with an
explicit, costly, one-way upgrade to fully random-access writable files.

See **[DESIGN.md](DESIGN.md)** for the full architecture, rationale, and
open questions.

## Repository layout

This is a Cargo workspace laid out along the architecture's component
boundaries. Everything here is a **design skeleton**: types, traits, and
the core state-machine logic (placement, append-only/writable transitions)
are real and tested; the I/O, FUSE, and RPC integration points are stubs
noting which crate (`fuser`, `io_uring`, `redb`, `tonic`, ...) a full
implementation would use, per DESIGN.md §4.

| Crate | Responsibility |
|---|---|
| `tartine-proto` | Shared on-disk/wire types: pool map, inode record, disk superblock, metadata ops. |
| `tartine-core` | Disk I/O abstraction, append-only chunk-log segment format, HRW placement (real, tested implementation). |
| `tartine-meta` | Metadata engine: inode table / directory tree store trait, synchronous 2-disk WAL replication protocol. |
| `tartine-fuse` | FUSE-facing write-path state machine (append-only ⇄ converting ⇄ writable) and the `ioctl` surface. |
| `tartined` | The daemon binary that wires the above into a running pool + FUSE mount + control socket. |
| `tartinectl` | CLI over the control socket (`disk add/remove`, `meta set-disks`, `file set-replication`, `convert`, ...). |

## Building

```sh
cargo build --workspace
cargo test --workspace
```

No external crates are required to build this skeleton (see the comment
block in the workspace `Cargo.toml` for what a full implementation would
add). `tartined`/`tartinectl` currently just print what they'd do — there
is no working mount yet.
