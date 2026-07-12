# TartineFS — Design Document

A Linux filesystem that pools together an arbitrary, changing set of disks
into a single namespace, replicates file data per-file and metadata on a
small, reconfigurable set of disks, and gives files copy-on-append,
log-structured semantics by default with an explicit (costly) upgrade path
to fully random-access writable files.

Status: design draft. Target: single Linux host, local block devices.

## 1. Goals / non-goals

**Goals**

- Disks (block devices) can be added to and removed from a *pool* while the
  filesystem is mounted and in use, without downtime.
- Every file has an independently configurable replication factor (how many
  disks hold a copy of its data).
- Filesystem metadata (namespace, inode table, chunk maps) is replicated
  synchronously across exactly **2** disks by default, and the pair of disks
  backing metadata can be changed (planned migration or failure-driven
  replacement) without downtime.
- Files are **append-only at birth**. An explicit operation (`ioctl`, xattr,
  or CLI) converts a file to a normal, in-place-writable POSIX file. This
  conversion is allowed to be expensive — it is not on any hot path.
- Self-healing: disk loss is detected, under-replicated data and metadata
  are automatically repaired from surviving copies.
- Runs entirely in userspace on stock Linux, using modern async I/O.

**Non-goals (v1)**

- Multi-host / networked cluster operation. Every disk is locally attached
  to the one host running the daemon. (§13 sketches the extension.)
- POSIX byte-range mandatory locking, full extended-ACL semantics — basic
  POSIX permissions/xattrs only.
- Snapshots/clones (natural future extension given the log-structured
  layout, not designed here).

## 2. Why this is a *pooling* filesystem, not a distributed one

The requirements talk about "disks", not "nodes" or "servers", and metadata
lives on a *fixed count of 2 disks*, not a quorum-sized cluster. That maps
directly onto a well-understood, much simpler problem than a distributed
filesystem: **one daemon process is the single writer and arbiter for the
whole pool**, and disks are dumb, locally-attached storage targets it reads
and writes directly (like `mdadm`, ZFS, or Btrfs multi-device, but with
per-file replication instead of whole-pool RAID levels).

This matters a lot for the metadata design: because there is exactly one
writer, we do **not** need distributed consensus (Raft/Paxos) to keep two
metadata replicas consistent. A synchronous primary→backup WAL ship with a
fencing epoch is sufficient and is what real single-node mirrored designs
(ZFS's mirrored ZIL, mdadm RAID-1) already do. §13 discusses what changes if
this daemon is later made highly-available across two hosts.

## 3. High-level architecture

```
                         ┌───────────────────────────┐
   user process          │        tartined            │
   (open/read/write) ──▶ │  ┌───────────────────────┐ │
        via VFS          │  │   FUSE session (fuser) │ │
                         │  └──────────┬────────────┘ │
                         │             ▼               │
                         │   ┌───────────────────┐     │
                         │   │   namespace / VFS  │     │
                         │   │   op dispatcher     │     │
                         │   └─────────┬─────────┘     │
                         │             ▼                │
                         │  ┌────────────────────────┐  │
                         │  │  metadata engine        │  │
                         │  │  (inode table, dirtree,  │  │
                         │  │   chunk maps, WAL)       │  │
                         │  └───────────┬──────────────┘ │
                         │              ▼                 │
                         │  ┌─────────────────────────┐   │
                         │  │  pool manager             │   │
                         │  │  (placement, membership,  │   │
                         │  │   rebalancer, scrubber)    │  │
                         │  └────────────┬──────────────┘  │
                         │               ▼                  │
                         │   ┌──────────────────────────┐   │
                         │   │  disk I/O layer (io_uring) │  │
                         │   └────┬────┬────┬────┬───────┘   │
                         └────────┼────┼────┼────┼───────────┘
                                  ▼    ▼    ▼    ▼
                               disk1 disk2 disk3 disk4 ...   (pool)

  control plane: tartinectl  ──unix socket (JSON/gRPC)──▶  tartined
```

`tartined` is the only process that opens the raw block devices. It exposes
the namespace to the kernel via FUSE (`/dev/fuse`), and exposes pool/file
administration via a local control socket used by the `tartinectl` CLI (and
by the `ioctl`/xattr handlers, which are just sugar over the same control
API executed inline on the FUSE session).

## 4. Technology choices

| Concern | Choice | Why |
|---|---|---|
| Filesystem frontend | [`fuser`](https://github.com/cberner/fuser) (Rust FUSE bindings), or `fuse3` for async | Memory-safe, fast to iterate, runs as a normal process. Linux 6.9+ `FUSE_PASSTHROUGH` and the `io_uring`-backed FUSE queue close most of the historical FUSE performance gap for large sequential I/O, which is the dominant pattern here (append + eventual materialization). |
| Disk I/O | `io_uring` (via the `io-uring` crate directly, or `tokio-uring`) with `O_DIRECT` | Zero-copy, batched, async submission to many disks concurrently; avoids double-buffering through the page cache for large append/replication writes. |
| Embedded metadata store | [`redb`](https://github.com/cberner/redb) (pure-Rust, MVCC, ACID, file-backed B-tree) | No unsafe, no C dependency, transactional — a good fit to be the on-disk engine underneath the metadata WAL/replica. `sled` is a fallback if redb's write-amplification profile is unacceptable. |
| Serialization | `serde` + `bincode` for on-disk records, `prost` (protobuf) for the control-plane RPC | Stable, fast, well understood. |
| Checksums | `blake3` (data blocks, cheap enough to run inline, strong integrity), `crc32c` (per-I/O quick check on the hot path, hardware accelerated) | Matches modern CoW filesystems (ZFS, Btrfs) practice of checksum-everything + scrub. |
| Placement | Rendezvous hashing (HRW), hand-rolled | O(1) to compute, minimal data movement on pool membership change, no need to gossip a full CRUSH-like map — see §7. |
| Async runtime | `tokio` | Ubiquitous, integrates with `io-uring` crates and gRPC (`tonic`) for the control plane. |
| Control-plane RPC | `tonic` (gRPC) over a Unix domain socket | Typed, streamable (useful for progress-reporting long operations like conversion or drain), local-only. |

Rust is a good fit for the whole system: this is exactly the class of
software (unsafe raw I/O, concurrent state machines, on-disk format
correctness) where Rust's guarantees pay for themselves, and the modern
crate ecosystem (`redb`, `io-uring`, `fuser`, `blake3`) already covers every
major component without needing a C dependency.

## 5. On-disk layout

### 5.1 Disk roles

A disk joining the pool is formatted with a small superblock and then used
in one (or both) of two roles, tracked in the pool map:

- **data role**: stores chunk-log segments for file data.
- **metadata role**: stores a full replica of the metadata store (`redb`
  file) for a metadata group it belongs to. In v1 there is exactly one
  metadata group, backed by exactly 2 disks.

A disk can hold both roles simultaneously (common case for small pools); at
larger scale operators will typically dedicate a couple of fast disks
(NVMe) to the metadata role.

### 5.2 Superblock (first 4 KiB of the device)

```rust
struct SuperBlock {
    magic: [u8; 8],          // "TARTINE1"
    pool_id: Uuid,           // identifies the pool this disk belongs to
    disk_id: Uuid,           // stable identity, independent of /dev/* path
    roles: DiskRoles,        // bitflags: DATA, METADATA
    format_version: u32,
    created_at: u64,
    // epoch of the pool map this disk last observed; used on daemon
    // startup to detect a disk that missed membership changes while
    // offline (e.g. was unplugged) and needs catch-up/resync.
    last_seen_epoch: u64,
    checksum: u32,           // crc32c of the rest of the block
}
```

### 5.3 Data role: chunk-log segments

File data is never written in place while a file is append-only (§9). Each
data disk manages a directory of fixed-size **segments** (default 256 MiB),
written strictly sequentially:

```
segment file: [ SegmentHeader | Record | Record | Record | ... ]

Record {
    inode: u64,
    chunk_seq: u64,       // monotonic per-inode chunk sequence number
    len: u32,
    checksum: u64,        // blake3 (truncated) of payload
    payload: [u8; len],
}
```

This is a Bitcask/Kafka-style append log: writes are pure sequential
`io_uring` writes (great for HDD and SSD alike, and trivially batchable).
Segments are immutable once sealed (full or the file was closed); a
background **compactor** reclaims space from segments whose records have
been superseded (file deleted, truncated, or *materialized* by the
append→writable conversion, §9.3) by copying live records forward into a
fresh segment and freeing the old one.

Once a file is converted to writable, its data moves out of the chunk-log
into **fixed-size block extents** (default 128 KiB, `O_DIRECT`-aligned) laid
out in a conventional extent allocator (a per-disk free-space bitmap/B-tree)
so that random `pwrite`/`mmap` writes are simple in-place block writes, same
as any conventional filesystem.

### 5.4 Metadata role: replicated store

Each metadata-role disk holds a `redb` database file containing:

- **inode table**: `inode_id -> InodeRecord` (mode flags including
  `AppendOnly | Converting | Writable`, size, replication_factor, owner,
  timestamps, xattrs, and either a chunk-log pointer list or an extent map
  depending on state).
- **directory tree**: `(parent_inode, name) -> inode_id`, plus a reverse
  `inode_id -> (parent_inode, name)` for `..`/hardlink accounting.
- **pool map**: current disk membership, roles, epoch, health state.
- **WAL**: see §6.

## 6. Metadata replication (2 disks, reconfigurable)

Given the single-writer model (§2), metadata replication is **synchronous
primary/backup log shipping with a fencing epoch**, not consensus:

1. The daemon holds an in-memory `MetaGroup { primary: DiskId, backup: DiskId, epoch: u64 }`.
2. Every metadata mutation (create, unlink, rename, chunk-map update,
   pool-map change, replication-factor change, ...) is first serialized as a
   `MetaOp` and appended to a WAL record.
3. The WAL record is written with `io_uring` to **both** disks concurrently
   and the operation is not acknowledged to the caller (i.e. the FUSE call
   does not return) until both writes are durable (`fdatasync`), or until
   the backup is declared dead by the health monitor and the group
   fails over to primary-only mode (degraded; see below).
4. Periodically (or on WAL size threshold) the WAL is checkpointed into the
   `redb` B-tree on both disks and truncated.

This gives the same durability guarantee as synchronous RAID-1 for
metadata: either disk alone has everything needed to reconstruct current
state.

**Changing the metadata disk pair** (`tartinectl meta set-disks <a> <b>`,
or automatic on failure):

- *Planned migration* (e.g. move metadata off an aging disk): add the new
  disk as a third, temporary replication target, let it catch up from a
  full copy + WAL replay while continuing to serve, then atomically bump
  `epoch` and drop the old disk from the group. No downtime, no window
  without 2 durable copies.
- *Failure* (a metadata disk drops out): the health monitor detects missed
  I/O / device removal, immediately fences the dead disk out of the group
  (`epoch += 1`, group becomes primary-only), keeps serving writes
  (degraded — only 1 durable copy) while it asynchronously provisions a
  replacement backup from the pool (best free space / lowest utilization)
  and streams a full resync. This mirrors how ZFS handles a mirrored ZIL
  vdev losing a member.
- On daemon restart, it reads the superblock `last_seen_epoch` +
  a small fixed well-known location pointer on *every* disk in the pool
  (not just metadata disks) that records "who is the current metadata
  group" — this bootstrap pointer itself is written with the same
  synchronous 2-disk rule, so it's always discoverable even if the disk you
  probe first is neither current metadata replica. Practically: each disk's
  superblock carries a cached copy of the last known `MetaGroup`; the
  daemon unions what it finds across all present disks and picks the
  highest-epoch, majority-agreeing answer to open.

**Scaling beyond one metadata group** (future, not required by the spec but
worth noting): shard the namespace by directory hash into N independent
2-disk metadata groups instead of one. The mechanism above is unchanged per
shard; only the routing (`shard = hash(parent_inode) % N`) is added. v1
ships with N=1 since "2 disks" was specified as a pool-wide property.

## 7. Pool management & placement

### 7.1 Pool map

```rust
struct PoolMap {
    epoch: u64,
    disks: HashMap<DiskId, DiskEntry>,
}

struct DiskEntry {
    roles: DiskRoles,
    state: DiskState,      // Active, Draining, Dead, Repairing
    weight: f64,            // relative capacity, default proportional to size
    used_bytes: u64,
    capacity_bytes: u64,
}
```

The pool map is itself just another row in the metadata store, replicated
like everything else (§6), so it survives daemon restarts and is
immediately consistent for all placement decisions.

### 7.2 Placement: rendezvous hashing (HRW), not CRUSH

For each file, data placement needs to answer: *given replication factor
N, which N (of the currently active) data disks hold this chunk?* We use
**highest random weight (rendezvous) hashing**:

```
score(disk, key) = hash(disk.id || key) * weight_factor(disk)
targets(key, n)  = top-n disks by score(disk, key), disk.state == Active
```

Properties that make this a good fit for a *single-node, dynamically
resized* pool (as opposed to CRUSH, which is designed for the harder
multi-node/rack/datacenter failure-domain problem Ceph solves):

- Deterministic, computed on the fly from the current pool map — no
  separate placement metadata to store per chunk beyond "which disks", and
  even that is only needed because reads must find data *now*, not because
  placement itself needs to be recorded to be reproducible.
- **Minimal disruption**: adding or removing one disk changes the target
  set for only `~1/|disks|` of existing keys (the defining property of HRW),
  so growing/shrinking the pool triggers proportionally small rebalancing,
  not a full reshuffle.
- `key` is `(inode_id, chunk_seq)` for data chunks, so each chunk of a large
  file can land on a different subset of disks — spreading a single hot
  file across the whole pool's bandwidth rather than pinning it to N disks.

### 7.3 Adding a disk

`tartinectl disk add /dev/nvme3n1 [--role data,metadata] [--weight ...]`

1. Write a fresh superblock (new `disk_id`, joins `pool_id`).
2. Metadata transaction: insert into `PoolMap`, `epoch += 1`.
3. Rebalancer wakes up, recomputes HRW target sets for existing chunks
   whose top-N changed as a result of the new disk being present, and
   copies the affected chunks onto it (throttled, background, I/O-priority
   below foreground traffic).

### 7.4 Removing a disk

`tartinectl disk remove <disk_id> --drain` (graceful) or `--force` (disk
already gone/dead):

- **Drain**: mark `Draining` (still readable, no new writes targeted at
  it). Rebalancer walks every chunk/metadata-replica it holds, re-places it
  per the *n-1-disk* HRW view, copies to the new target, verifies, then
  marks the disk `Dead`/removable. `tartinectl` reports progress and blocks
  (or returns immediately with `--async`) until drain completes.
- **Force** (disk failed): mark `Dead` immediately. Every file whose
  replication factor was N and had a copy on the dead disk is now at N-1
  on that chunk; the scrubber/repair loop treats this exactly like a
  checksum-failure repair (§8) — re-replicate from a surviving copy to a
  freshly HRW-selected disk. If the dead disk was in the metadata group,
  §6's failure path kicks in.
- If removing a disk would drop total pool capacity/weight below what's
  needed to satisfy some file's configured replication factor, the CLI
  warns and requires `--force` to proceed (files stay at reduced
  replication rather than the operation being silently accepted).

## 8. Integrity: checksums, scrubbing, repair

- Every chunk-log record and every fixed-size extent block carries a
  BLAKE3 checksum, verified on every read.
- A background **scrubber** walks all disks at a throttled rate, re-reads
  every record/block, verifies checksums, and compares replicas: on
  mismatch (bit rot) or a missing replica (previous disk loss not yet
  fully repaired), it repairs from a healthy copy and re-checksums.
- Reads that hit a checksum failure transparently retry against another
  replica (if `replication_factor > 1`) before surfacing an error to the
  application, and kick the repair loop for that chunk immediately rather
  than waiting for the scrubber's turn.

## 9. Append-only files and the writable conversion

### 9.1 Default state: `AppendOnly`

Every newly created file starts with `InodeRecord.mode = AppendOnly`. In
this state:

- `write()` is only accepted at the current end-of-file offset — enforced
  by the daemon regardless of the flags the client opened with (i.e. even
  without `O_APPEND`, non-append writes are rejected), matching the
  "append-only at birth" requirement rather than relying on the calling
  process to behave.
- `ftruncate`, `pwrite` to non-EOF offsets, and writable `mmap` all fail
  with `EPERM`.
- Each accepted append becomes one `Record` (§5.3) in the chunk-log,
  replicated synchronously to `replication_factor` data disks chosen by
  HRW on `(inode, chunk_seq)`. This is cheap: pure sequential writes, no
  read-modify-write, no extent allocation — a good match for the
  "append-only" restriction actually buying something operationally, not
  just being a POSIX-compatibility annoyance.
- The inode's chunk-log pointer list (`Vec<(chunk_seq, [DiskId; N])>`) is
  what the metadata store tracks; reads reconstruct the file by walking
  this list in order.

### 9.2 Triggering the conversion

Three equivalent entry points, all routed to the same daemon-side
operation:

```c
/* ioctl, defined in a small tartine_ioctl.h header shipped with the fs */
#define TARTINE_IOC_MAGIC        'T'
#define TARTINE_IOC_MAKE_WRITABLE   _IOW(TARTINE_IOC_MAGIC, 1, __u32 /* flags */)
#define TARTINE_IOC_GET_STATE       _IOR(TARTINE_IOC_MAGIC, 2, struct tartine_state)

struct tartine_state {
    __u32 mode;        /* TARTINE_MODE_APPEND_ONLY | _CONVERTING | _WRITABLE */
    __u64 bytes_total;
    __u64 bytes_converted;   /* progress while _CONVERTING */
};
```

- `ioctl(fd, TARTINE_IOC_MAKE_WRITABLE, &flags)` — `flags` bit 0 selects
  synchronous (blocks the ioctl until conversion is fully complete) vs
  asynchronous (returns immediately, state becomes `Converting`, caller
  polls `TARTINE_IOC_GET_STATE`). FUSE's `ioctl` op maps straightforwardly
  to this.
- `setxattr(path, "user.tartine.writable", "1", ...)` — convenience for
  tools that can't easily issue raw ioctls (shell scripts via
  `setfattr`); same async behavior as the default ioctl.
- `tartinectl convert <path> [--wait]` — CLI wrapper over the control
  socket, useful for batch/offline conversion and for scripting.

### 9.3 What conversion actually does (and why it's costly)

The chunk-log format is great for appends and terrible for random writes
(variable-length records, scattered across segments and possibly disks,
no fixed block addressing). Conversion **materializes** the file into the
conventional fixed-size extent layout used by writable files (§5.3):

1. Mark inode `Converting` (metadata txn — this alone is cheap and is what
   makes the ioctl able to return quickly in async mode).
2. Allocate a fresh extent map sized to the file's current length, on
   `replication_factor` disks chosen by HRW over `(inode, extent_index)`.
3. Stream-read the chunk-log records in order (fan-in from wherever HRW
   had scattered them) and stream-write them into the new fixed-size
   aligned extents (fan-out to the replica set), verifying checksums both
   ways. This is a full O(file size) read + O(file size × N) write — the
   expensive part, intentionally so per the requirement — done as
   throttled background I/O so it doesn't starve foreground traffic.
4. While `Converting`, the file **remains fully readable** (reads are
   served from the old chunk-log until the swap in step 5) and **remains
   append-writable at its old EOF** (new appends land as additional
   chunk-log records past the point already captured in step 3's
   snapshot; the materializer picks them up in a final small delta pass)
   — so a long-running append workload isn't blocked by a slow conversion.
   Any attempt at a *random* write during `Converting` blocks (or returns
   `EAGAIN` in non-blocking mode) until the swap completes, since it's not
   yet safe to accept it.
5. Atomic metadata transaction: swap the inode's data-locator from
   chunk-log-pointer-list to extent-map, set `mode = Writable`.
6. Old chunk-log segments' records for this inode are now dead; the
   compactor reclaims that space on its normal schedule (no need to do it
   synchronously — the costly part was the copy, not the cleanup).

After step 5 the file is a completely ordinary writable file: `pwrite` at
any offset, `ftruncate`, writable `mmap` all work via normal extent
read/modify/write against the (still per-file-configurable, §10) replica
set. There is no path back to `AppendOnly` — the conversion is one-way, which
keeps the write-path invariants simple (a file's mode fully determines
which of the two very different write paths applies, with no third
"has-been-converted-back" case to reason about).

## 10. Per-file replication factor

`InodeRecord.replication_factor: u8` — set at creation from a
directory/pool default (`tartinectl fs set-default-replication N`), and
changeable per file at any time:

```
tartinectl file set-replication <path> <N>
# or: setfattr -n user.tartine.replication -v N <path>
```

Changing it doesn't move data synchronously — it just updates the target N
in metadata and lets the same rebalancer used for disk add/remove (§7.3)
converge the file's actual replica count to the new target in the
background (add replicas by copying from an existing one via HRW; remove
excess replicas by deleting from the disks no longer in the top-N set).
This reuses one mechanism for "pool topology changed" and "this file's
policy changed" instead of two.

## 11. Data path summary

**Append write** (`AppendOnly` file): FUSE `write` → daemon validates
offset == EOF and mode → HRW picks N data disks for `(inode, chunk_seq)` →
parallel `io_uring` writes to all N (fan-out, wait per configured
consistency: `all` by default, `quorum` optional for lower tail latency at
reduced durability) → on success, metadata txn appends the chunk pointer
(§6, synchronous 2-disk WAL) → ack to caller.

**Read**: FUSE `read` → resolve offset to chunk-log record(s) or extent(s)
from metadata (in-memory cache, backed by the redb store) → pick one live
replica (prefer local/least-loaded disk) → `io_uring` read + checksum
verify → on checksum failure, retry next replica + kick repair.

**Random write** (`Writable` file only): FUSE `write` at arbitrary offset →
resolve/allocate extents → read-modify-write if sub-block, else direct
overwrite → fan-out to replica set → metadata txn only needed if the
extent map changed (new allocation, size growth), not per write.

## 12. Failure & recovery matrix

| Event | Detection | Response |
|---|---|---|
| Data disk disappears | I/O error / device removal uevent | Mark `Dead` in pool map (metadata txn); chunks/extents it held are under-replicated → repair loop re-replicates from survivors to newly HRW-selected disks. |
| Metadata disk disappears | I/O error on WAL write | Fence out of `MetaGroup`, `epoch += 1`, continue degraded (primary-only) while provisioning + resyncing a replacement (§6). |
| Both metadata disks gone simultaneously | Daemon fails to open pool | Manual recovery: operator points daemon at any surviving data disks; namespace is unrecoverable beyond what can be reconstructed from data-disk superblocks' cached `MetaGroup`/pool-map echoes (best-effort) — this is the one true single point of failure in v1, called out explicitly in §14. |
| Daemon crash / power loss mid-write | On restart, WAL replay | Data disks: incomplete trailing record in a segment is detected via checksum/length sanity and truncated (standard log-structured recovery). Metadata: WAL is replayed from last checkpoint on both metadata disks (they agree, since writes were synchronous); any op whose data-write never got acked is simply absent from the WAL and never happened. |
| Bit rot | Scrubber / read-time checksum mismatch | Repair from healthy replica (§8). |
| Conversion interrupted (crash mid-`Converting`) | Inode `mode == Converting` found on restart | Discard partial extent map, resume from step 2 of §9.3 (old chunk-log is still intact and authoritative until the atomic swap in step 5, so this is always safe to restart from scratch). |

## 13. Explicit non-goal: multi-host HA

Everything above assumes one daemon process owns the pool. If the
requirement ever grows to "survive losing the whole host", the natural
extension is an active/standby pair of `tartined` processes on two hosts
sharing access to the same disks (multi-initiator NVMe-oF/iSCSI, or simply
each disk being dual-ported), with leader election (now genuinely needing
consensus, e.g. `openraft`) over *who is allowed to write* — at that point
the "2 disks for metadata" constraint would need to be revisited alongside
"2 hosts for the writer role" as a related but distinct decision. Called
out here so the line between what's designed and what's future work is
explicit.

## 14. Known limitations / open questions

- **Single daemon is a SPOF for availability** (not for durability — data
  survives; the pool just can't be *served* while the daemon is down).
  Acceptable for v1's stated scope; §13 is the extension path.
- Losing both metadata disks at once loses the namespace even though file
  data may largely survive on data disks — same failure mode as losing
  both sides of any RAID-1, just for metadata instead of data. Mitigation:
  make it hard to do by construction (`tartinectl` refuses to let the two
  metadata disks be, e.g., partitions of the same physical device, and
  warns if they share an obvious failure domain like a USB hub or PSU rail
  where that's detectable).
- Fixed extent size (128 KiB default) for writable files is a simple
  starting point; a real implementation would likely want variable extent
  sizing or a B-tree of extents for very sparse/very large writable files.
- The conversion's "still append-writable during `Converting`" behavior
  (§9.3 step 4) adds real complexity (two write paths must agree on where
  the boundary between "already materialized" and "still tail-appending"
  is); a simpler v1 could instead just block *all* writes during
  conversion and document that as the cost of "costly", deferring the
  concurrent-append refinement.
- No mandatory locking / range locking specified — POSIX advisory locks
  (`flock`/`fcntl`) can be layered on the FUSE session using the daemon's
  single-writer position as the natural lock authority, but that's not
  designed here.

## 15. Suggested milestones

1. `tartine-core`: disk superblock format, chunk-log segment read/write,
   HRW placement — unit-testable without FUSE at all.
2. `tartine-meta`: `redb`-backed inode table + directory tree + WAL, single
   in-process metadata group (no replication yet) — enough for a
   single-disk, non-replicated prototype.
3. `tartine-fuse` + `tartined`: wire it up to a real mountpoint, append-only
   writes and reads working end-to-end on one disk.
4. Add the second metadata disk + synchronous WAL shipping (§6).
5. Multi-disk pool + HRW-driven placement + `tartinectl disk add/remove`
   (§7).
6. Per-file replication factor + rebalancer (§10).
7. The `ioctl` conversion path (§9) — last, since everything above needs
   to be solid before "costly, one-way, background materialization" is
   safe to build on.
8. Scrubber/repair loop, disk-failure handling (§8, §12).
