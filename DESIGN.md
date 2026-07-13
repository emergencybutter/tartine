# TartineFS — Design Document

A Linux filesystem that pools together an arbitrary, changing set of disks
into a single namespace, replicates file data per-file and metadata on a
small, reconfigurable set of disks, and gives files copy-on-append,
log-structured semantics by default with an explicit (costly) upgrade path
to fully random-access writable files.

Status: design draft. Target: single Linux host, local block devices.

**Production target is an in-kernel filesystem** (`kernel/`, C, registered
as a real `file_system_type` — `mount -t tartine`), not FUSE. FUSE
(`crates/tartine-fuse`, `crates/tartined`) is a deliberate prototyping
step: the design is validated end-to-end in userspace, where mistakes are
cheap, before it's committed to kernel code, where they aren't. The two
front ends share one implementation of the parts worth getting bit-for-bit
identical — placement and the append-only/writable state machine — via
`crates/tartine-kcore`, a dependency-free `#![no_std]` Rust crate compiled
both as a plain Rust dependency (FUSE prototype) and as a `staticlib`
linked into the C kernel module (production). See §3 and §4 for the full
shape of this, and `kernel/README.md` for exactly what in the kernel
module is real versus stubbed today.

## 1. Goals / non-goals

**Goals**

- Disks (block devices) can be added to and removed from a *pool* while the
  filesystem is mounted and in use, without downtime.
- Every file has an independently configurable redundancy policy: how
  many replicas, which disk class(es) they must land on, and optionally
  a specific disk to pin a replica to (§10).
- Filesystem metadata (namespace, inode table, chunk maps) is replicated
  synchronously across exactly **2** disks by default, and the pair of disks
  backing metadata can be changed (planned migration or failure-driven
  replacement) without downtime.
- Files are **append-only at birth**. An explicit operation (`ioctl`, xattr,
  or CLI) converts a file to a normal, in-place-writable POSIX file. This
  conversion is allowed to be expensive — it is not on any hot path.
- Self-healing: disk loss is detected, under-replicated data and metadata
  are automatically repaired from surviving copies.
- Runs as a native Linux kernel filesystem (`mount -t tartine`), with no
  userspace daemon required to operate — see §3.

**Non-goals (v1)**

- Multi-host / networked cluster operation. Every disk is locally attached
  to the one host. (§13 sketches the extension.)
- POSIX byte-range mandatory locking, full extended-ACL semantics — basic
  POSIX permissions/xattrs only.
- Snapshots/clones — not designed here, but the chunk-log layout makes
  them nearly free for append-only files, so they're tracked as a cheap
  follow-on rather than a distant maybe (§16.5).

## 2. Why this is a *pooling* filesystem, not a distributed one

The requirements talk about "disks", not "nodes" or "servers", and metadata
lives on a *fixed count of 2 disks*, not a quorum-sized cluster. That maps
directly onto a well-understood, much simpler problem than a distributed
filesystem: **the mounted kernel module is the single writer and arbiter
for the whole pool**, and disks are dumb, locally-attached storage targets
it reads and writes directly (like `mdadm`, ZFS, or Btrfs multi-device, but
with per-file replication instead of whole-pool RAID levels).

This matters a lot for the metadata design: because there is exactly one
writer, we do **not** need distributed consensus (Raft/Paxos) to keep two
metadata replicas consistent. A synchronous primary→backup WAL ship with a
fencing epoch is sufficient and is what real single-node mirrored designs
(ZFS's mirrored ZIL, mdadm RAID-1) already do. §13 discusses what changes if
this were later made highly-available across two hosts.

## 3. High-level architecture

TartineFS is **Btrfs-shaped, not Ceph-shaped**: one kernel module owns
everything — pool map, metadata, and the data path — the same way
ext4/btrfs/xfs do. There is no daemon the filesystem depends on to mount
or operate; `tartinectl` is a thin CLI that issues `ioctl(2)`s, exactly
like `btrfs-progs` talks to btrfs. This was a deliberate choice over a
"thin kernel client + always-on userspace control-plane daemon" split
(the alternative considered — see the note at the end of this section):
depending on a userspace process for anything beyond serving
already-cached data is a materially different reliability story than
every other production Linux filesystem, and this design doesn't need
that trade for anything it's trying to accomplish.

```
                    ┌───────────────────────────────────────────────┐
  user process      │               kernel: tartine.ko                │
  (open/read/  ───▶ │  ┌─────────────────────────────────────────┐  │
   write, ioctl)     via VFS: superblock / inode / file_operations   │
                    │  └────────────────────┬────────────────────┘  │
                    │                        ▼                       │
                    │   ┌─────────────────────────────────────────┐ │
                    │   │  write-path state machine + placement     │ │
                    │   │  (tartine-kcore, linked in as a static    │ │
                    │   │   lib — §4)                                 │ │
                    │   └────────────────────┬────────────────────┘ │
                    │                        ▼                       │
                    │   ┌─────────────────────────────────────────┐ │
                    │   │  metadata (in-kernel on-disk B-tree,       │ │
                    │   │  2-disk synchronous WAL — §6)               │ │
                    │   └────────────────────┬────────────────────┘ │
                    │                        ▼                       │
                    │   ┌─────────────────────────────────────────┐ │
                    │   │  block I/O (bio / blk-mq)                  │ │
                    │   └───┬────────┬────────┬────────┬──────────┘ │
                    │       ▼        ▼        ▼        ▼             │
                    └───────┼────────┼────────┼────────┼─────────────┘
                           disk1    disk2    disk3    disk4 ...   (pool)

  admin: tartinectl  ──ioctl(2) on the mountpoint, or on /dev/tartine-ctl
                         pre-mount for device scan ──▶  tartine.ko
```

`tartine.ko` is the only thing that opens the raw block devices. Pool-wide
administration (disk add/remove, per-file replication factor, the
append→writable conversion trigger) is all `ioctl(2)` on the mounted
filesystem, handled synchronously or by a kernel workqueue for anything
that runs in the background (rebalancing, scrubbing, materialization).
Before a pool's first disk is mounted, a small control device
(`/dev/tartine-ctl`) accepts a "scan this device" ioctl so the module can
learn which block devices belong to which pool — the same role
`btrfs device scan` plays for btrfs multi-device pools, needed because a
pool spans multiple device nodes and a plain `mount /dev/sda1 /mnt` only
names one of them (see `kernel/tartine.h`'s `TARTINE_CTL_IOC_SCAN_DEVICE`).

**Why not a thin kernel client + userspace control daemon instead** (the
alternative seriously considered before committing to the all-in-kernel
design above): it would mean far less new kernel code — the module would
only need to handle the VFS/data-I/O hot path using placement/metadata
state pushed down from userspace, while a daemon like the FUSE
prototype's `tartined` kept owning pool topology changes, metadata WAL
replication, and rebalancing. That's a real reduction in kernel-code
risk. It was set aside because it reintroduces exactly the dependency
this design is trying to avoid: the filesystem would work for
already-cached reads even with the daemon down, but anything involving
pool topology or metadata durability would not — a materially different
promise than "you can always `mount -t tartine` and get a working
filesystem," which is what every other production Linux filesystem
guarantees and what this design commits to as well.


## 4. Technology choices

### 4.1 Production (`kernel/`): C + a `no_std` Rust core

| Concern | Choice | Why |
|---|---|---|
| VFS glue (superblock/inode/`file_operations`), kbuild integration | C | Full, mature access to the VFS API and block layer with no abstraction gap to fight. Rust-for-Linux's bindings for *drivers* are solid, but its bindings for building a full custom *local filesystem* (as opposed to a device driver) are still nascent as of this writing — betting the VFS glue itself on them would mean building or upstreaming missing abstractions as part of this project, on top of designing the filesystem. Not worth the combined risk for a first kernel version. |
| Placement (HRW) and the append-only/converting/writable state machine | `crates/tartine-kcore`, `#![no_std]`, zero dependencies, compiled as a `staticlib` and linked into the `.ko` | This is the logic worth isolating in a memory-safe, unit-tested language: pure functions, no allocation, no floating point (see §4.2), small enough to fully specify and test outside the kernel. It's also the *one* implementation both the kernel module and the FUSE prototype run — see §4.2. |
| On-disk metadata store | Purpose-built on-disk B-tree against the block layer directly (C) — **not** an embedded userspace KV engine | Nothing like `redb`/`sled` (they assume `std`, `mmap` of regular files) is usable from kernel context. This is the same problem Btrfs/XFS solved for themselves; there's no shortcut, the metadata engine has to be written against `struct bio`/the block layer like the rest of a kernel filesystem. Not implemented yet — see `kernel/README.md`'s scope notes and §15's roadmap. |
| Disk I/O | `struct bio` submission via `blk-mq` | The kernel's own modern (multi-queue) async block I/O path — the in-kernel equivalent of what `io_uring` gives a userspace program, and the only option once the module is genuinely in-kernel (`io_uring` is a *userspace-facing* syscall interface; it has no meaning from inside a kernel module). |
| Checksums | Kernel's built-in `crc32c()` (`lib/crc32c.c`, hardware-accelerated where available) | Already provided, already fast, no reason to duplicate it in `tartine-kcore` — see that crate's doc comment for why checksumming was deliberately left out of the Rust core. |
| Placement key hashing | FNV-1a, hand-rolled in `tartine-kcore` | `core` has no `SipHash` (that's `std`-only); FNV-1a needs no crate and no cryptographic strength, just good distribution — see `tartine-kcore/src/hash.rs`. |
| Admin surface | `ioctl(2)` on the mountpoint (already-mounted pool) and on `/dev/tartine-ctl` (pre-mount device scan) | Same shape as `btrfs-progs`; no RPC framework, no daemon, no socket to manage. |

### 4.2 Prototype (`crates/tartine-fuse`, `crates/tartined`): Rust, FUSE

| Concern | Choice | Why |
|---|---|---|
| Filesystem frontend | [`fuser`](https://github.com/cberner/fuser) (Rust FUSE bindings) | Runs as a normal process, so the design — placement, replication, the append-only/writable state machine, failure handling — can be built and iterated on quickly and safely before any of it becomes kernel code, where mistakes are much more expensive. This is a means to validate the design, not an alternative production target (§3). |
| Disk I/O | `io_uring` / `tokio-uring`, `O_DIRECT` | The userspace equivalent of `blk-mq` above; fits a `std`-linked async prototype well. |
| Embedded metadata store | [`redb`](https://github.com/cberner/redb) | Lets the prototype validate the *metadata replication protocol* (§6) quickly without also hand-writing an on-disk B-tree in the prototype — that part of the kernel module's design (§4.1) is deliberately not re-derived here, since `redb` itself isn't what ships. |
| Shared logic | `tartine-kcore` (§4.1), consumed as a plain Rust dependency (its `freestanding` feature, and thus its `no_std`-ness, is off in this configuration) | The prototype calls the exact same compiled placement and write-path functions the kernel module will call through its C ABI — not a lookalike reimplementation that could quietly drift from what ships. `tartine-core`'s `targets()` (a `Vec`/`PoolMap`-friendly convenience wrapper) and `tartine-fuse`'s write-path adapter are both thin translation layers over `tartine-kcore`, not separate algorithms. |

Rust is a good fit for the whole system's *logic* — this is exactly the
class of code (concurrent state machines, on-disk format correctness)
where its guarantees pay for themselves — but "prefer Rust" runs into a
real limit at the VFS boundary itself: that layer needs the kernel's C
API surface either way, and Rust-for-Linux's coverage of it for full
custom filesystems isn't there yet. Splitting the system exactly at that
boundary (§4.1 vs. the parts of §4.1 carved into `tartine-kcore`) is what
lets this design get real Rust safety guarantees on the parts that
benefit most, without staking the whole kernel module on unproven
bindings.

## 5. On-disk layout

### 5.1 Disk roles

A disk joining the pool is formatted with a small superblock and then used
in one (or both) of two roles, tracked in the pool map:

- **data role**: stores chunk-log segments for file data.
- **metadata role**: stores a full replica of the metadata store — the
  in-kernel on-disk B-tree described in §4.1 (the FUSE prototype
  substitutes `redb` here for speed of iteration; see §4.2) — for a
  metadata group it belongs to. In v1 there is exactly one metadata
  group, backed by exactly 2 disks.

A disk can hold both roles simultaneously (common case for small pools); at
larger scale operators will typically dedicate a couple of fast disks
(NVMe) to the metadata role.

### 5.2 Superblock (first 4 KiB of the device)

This is what `kernel/tartine.h`'s `struct tartine_disk_super` and
`kernel/tartine_main.c`'s `tartine_fill_super()` actually implement
(`__packed`, explicit little-endian field widths — it crosses the disk
boundary, so no compiler is allowed to reinterpret it):

```rust
struct SuperBlock {
    magic: [u8; 8],          // "TARTINE1"
    pool_id: Uuid,           // identifies the pool this disk belongs to
    disk_id: Uuid,           // stable identity, independent of /dev/* path
    roles: DiskRoles,        // bitflags: DATA, METADATA
    format_version: u32,
    created_at: u64,
    // epoch of the pool map this disk last observed; used on mount to
    // detect a disk that missed membership changes while offline (e.g.
    // was unplugged) and needs catch-up/resync.
    last_seen_epoch: u64,
    checksum: u32,           // crc32c() of the rest of the block — the
                              // kernel's own, already hardware-accelerated
                              // where available; see §4.1.
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
    checksum: u32,        // crc32c() of payload (§4.1)
    payload: [u8; len],
}
```

This is a Bitcask/Kafka-style append log: writes are pure sequential
`bio` submissions via `blk-mq` in the kernel module (great for HDD and SSD
alike, and trivially batchable; the FUSE prototype's equivalent is
`io_uring`, §4.2). Segments are immutable once sealed (full or the file
was closed); a background **compactor** reclaims space from segments
whose records have been superseded (file deleted, truncated, or
*materialized* by the append→writable conversion, §9.3) by copying live
records forward into a fresh segment and freeing the old one.

Once a file is converted to writable, its data moves out of the chunk-log
into **fixed-size block extents** (default 128 KiB, block-aligned) laid
out in a conventional extent allocator (a per-disk free-space bitmap/B-tree)
so that random `pwrite`/`mmap` writes are simple in-place block writes, same
as any conventional filesystem.

`crates/tartine-core/src/segment.rs` implements this same record layout
for the FUSE prototype (as plain synchronous file I/O rather than `bio`).
It's independently written, not shared code the way placement/write-path
are (§4.2) — the two implementations agreeing on the on-disk format is a
spec-conformance property, not a code-sharing one. §15's roadmap flags
golden test vectors as the way to actually verify that agreement instead
of assuming it.

### 5.4 Metadata role: replicated store

Each metadata-role disk holds a metadata store (the in-kernel on-disk
B-tree in production, a `redb` database file in the FUSE prototype — §4)
containing:

- **inode table**: `inode_id -> InodeRecord` (mode flags including
  `AppendOnly | Converting | Writable`, size, redundancy policy (§10),
  owner, timestamps, xattrs, a `data_lost` flag (§8.3), and either a
  chunk-log pointer list or an extent map depending on state).
- **directory tree**: `(parent_inode, name) -> inode_id`, plus a reverse
  `inode_id -> (parent_inode, name)` for `..`/hardlink accounting.
- **pool map**: current disk membership, roles, epoch, health state.
- **WAL**: see §6.

## 6. Metadata replication (2 disks, reconfigurable)

Given the single-writer model (§2), metadata replication is **synchronous
primary/backup log shipping with a fencing epoch**, not consensus:

1. The mounted module holds an in-memory `MetaGroup { primary: DiskId, backup: DiskId, epoch: u64 }`.
2. Every metadata mutation (create, unlink, rename, chunk-map update,
   pool-map change, replication-factor change, ...) is first serialized as a
   `MetaOp` and appended to a WAL record.
3. The WAL record is written (via `bio` submission in production, `io_uring`
   in the FUSE prototype — §4) to **both** disks concurrently, and the
   operation is not acknowledged to the caller (the VFS call does not
   return) until both writes are durable, or until the backup is declared
   dead by the health monitor and the group fails over to primary-only
   mode (degraded; see below).
4. Periodically (or on WAL size threshold) the WAL is checkpointed into the
   metadata B-tree on both disks and truncated.

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

  **Fence durability ordering** (load-bearing — getting this wrong is an
  unrecoverable on-disk mistake): the fence record (new epoch + new
  membership) must be durable on the surviving metadata disk **and**
  acknowledged into the superblock echoes of a majority of the pool's
  disks *before the first degraded-mode write is acknowledged to any
  caller*. Without that ordering, a crash during degraded operation
  followed by the *old* backup reappearing leaves two self-consistent
  metadata stores whose epochs can tie — metadata split-brain, with
  recovery unable to tell which side carries the post-fence writes. With
  it, the majority of superblock echoes always names the surviving side
  before any write exists that only the surviving side has.
- On mount, the module reads the superblock `last_seen_epoch` + a small
  fixed well-known location pointer on *every* disk in the pool (not just
  metadata disks) that records "who is the current metadata group" — this
  bootstrap pointer itself is written with the same synchronous 2-disk
  rule, so it's always discoverable even if the disk you probe first is
  neither current metadata replica. Practically: each disk's superblock
  carries a cached copy of the last known `MetaGroup`; mount unions what
  it finds across all present disks (via the device-scan registry, §3)
  and opens the highest-epoch answer confirmed by a majority of echoes.
  If the surviving evidence is **ambiguous** — e.g. both metadata disks
  claim the same epoch with different membership, or no majority of
  echoes agrees — mount *refuses* with a clear error rather than
  guessing; an operator can override with an explicit
  `tartinectl pool adopt <disk>` that names which side to treat as
  authoritative (and thereby knowingly discards the other side's tail).

**Scaling beyond one metadata group** (future, not required by the spec but
worth noting): shard the namespace by directory hash into N independent
2-disk metadata groups instead of one. The mechanism above is unchanged per
shard; only the routing (`shard = hash(parent_inode) % N`) is added. v1
ships with N=1 since "2 disks" was specified as a pool-wide property.

**Single-disk pools**: the metadata replication factor is really
`min(2, disks in the pool)`, not a hard floor of exactly 2. A pool
formed with only 1 disk starts in the same primary-only state described
above for backup failure — one durable metadata copy, `MetaGroup.backup
= None` — except entered intentionally at format time instead of
reached via failure + fencing. That one disk necessarily holds both the
metadata role and the data role (§5.1 already allows a disk to hold
both). The moment a second disk joins the pool, the existing *planned
migration* path above applies unchanged: add it as the backup, let it
catch up via full copy + WAL replay, then bump the epoch — there is no
separate "graduate out of single-disk mode" mechanism to build.

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
like everything else (§6), so it survives unmount/remount and is
immediately consistent for all placement decisions.

### 7.2 Placement: rendezvous hashing (HRW), not CRUSH

For each file, data placement needs to answer: *given replication factor
N, which N (of the currently active) data disks hold this chunk?* We use
**logarithmic weighted rendezvous hashing** (Thaler–Ravishankar):

```
u(disk, key)     = hash(disk.id || key) / 2^64        // uniform in (0, 1]
score(disk, key) = weight(disk) / -ln(u(disk, key))
targets(key, n)  = top-n Active disks by score, skipping disks with no free space
```

A note on the weighting, because the obvious shortcut is wrong: scoring
with `hash * weight` does **not** give capacity-proportional placement —
for two disks weighted 2:1 it hands the heavier disk 75% of keys instead
of the proportional 66.7%, and the skew compounds as the pool grows, so
big disks fill even faster than their capacity justifies. The logarithmic
form above is exactly proportional (each disk wins with probability
`wᵢ/Σw`). Since `-ln u` and `-log2 u` differ only by a constant factor
that cancels in comparisons, `tartine-kcore` implements this FPU-free
with a fixed-point Q16 `-log2` (integer square-and-shift, exact bits, no
tables) — see `crates/tartine-kcore/src/placement.rs`, including a
statistical unit test whose tolerance band deliberately excludes the
biased shortcut's output.

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

**Full disks and ENOSPC**: HRW's ranking is treated as a *preference
list*, not a verdict — at write time the allocator walks down the ranked
list skipping disks that are full (or `Draining`/`Dead`), so a write only
fails with `ENOSPC` when fewer than N disks in the whole pool can accept
it. This costs nothing extra: the chosen replica set is recorded in the
chunk's metadata anyway (reads never recompute placement), and the
rebalancer treats "placed below HRW rank because the preferred disk was
full" exactly like any other deviation to converge later. `statfs`
reports **physical** pool capacity and free space; how many logical bytes
that translates to depends on each file's replication factor, which is
the honest answer (`df` on a pool of mixed RF files has no single logical
number).

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

### 8.1 Repair restores replication *intent*, not just a copy count

"Repair" means re-placing the missing/corrupt replica exactly where a
fresh write would put it today, not merely "copy it somewhere so there
are N copies again": the repair loop re-derives the target disk with the
same HRW computation (§7.2) over the chunk's `(inode, chunk_seq)` /
`(inode, extent_index)` key against the **current** pool map, honoring
the file's declared `RedundancyScheme` (§10) — a slot pinned to a disk
class or a specific disk stays pinned, an `AnyOfClass` slot gets
re-ranked from scratch. This is the same code path §7.4's "Force" disk
removal already describes ("re-place it per the *n-1-disk* HRW view");
§8's scrubber-driven repair and §7.4's disk-loss repair are the same
mechanism triggered by two different detectors (periodic scrub vs.
health-monitor disk-removal event), not two different behaviors to keep
in sync.

### 8.2 Repair prioritization: metadata before data

A disk can hold both the data and metadata roles (§5.1 — the common case
for small pools, and *always* the case for a single-disk pool, §6). When
one disk failure degrades both at once, **metadata resync is serviced
first**: the health monitor's fencing + backup-reprovisioning (§6,
"Failure") runs immediately and is not queued behind the data repair
loop's work for that same disk. This is a deliberate priority, not an
accident of implementation order — losing the *second* metadata disk
before a replacement finishes resyncing loses the entire namespace (§14's
"one true single point of failure"), while data that's merely
under-replicated (assuming its configured replication factor was ≥2 to
begin with) is at elevated risk of loss, not yet lost. The scrubber/
repair loop for the failed disk's data chunks still runs — it's queued
behind metadata resync, not skipped — and picks up as soon as the
health monitor has spare I/O priority to give it.

### 8.3 Total data loss: the `data_lost` inode flag

§8's repair loop assumes at least one surviving replica to repair *from*.
That assumption doesn't always hold:

- A file with `replication_factor == 1` (always true for every file on a
  single-disk pool, §6; possible on a larger pool via an explicit
  single-slot redundancy policy, §10) loses a chunk/extent outright the
  moment its one disk dies — there was never a second copy to fall back
  on.
- Even at `replication_factor > 1`, simultaneous loss of every disk
  holding a given chunk's replicas (rare, but the whole point of a
  failure matrix is enumerating the rare cases) has the same result.

When the repair loop (or a read-time checksum failure, §8) finds a
chunk/extent with **zero** surviving replicas — as opposed to merely
being unable to find a *new* disk to repair onto, which is the
pre-existing `Unplaceable`/`ENOSPC` case from §7.2 — that data is gone,
not degraded-with-repair-pending. In the same metadata transaction that
records the empty replica list, the inode's `data_lost` flag is set
(`InodeRecord.data_lost`, §5.4) — a permanent, on-disk fact about the
file, not a transient in-memory error, so it survives remount and is
visible to `TARTINE_IOC_GET_STATE` (§9.2) for monitoring tooling.

Setting `data_lost` puts the file into **read-only-metadata quarantine**:

- **Still permitted** — anything that only touches the inode's metadata,
  not its data: `stat`/`getattr` (so the file still shows up in `ls`,
  with its last-known size), `unlink` (`rm` always works — a file with no
  recoverable content shouldn't also be impossible to remove), and
  `chmod`/`chown`/xattr changes (ownership and permissions are metadata,
  same reasoning as §9.2's "conversion is restricted like `chmod`" — the
  operation itself doesn't touch a data disk, so there's no reason to
  block it).
- **Rejected with `EIO`** — anything that requires reading or writing
  file content: `read`, `write`/`pwrite` (append or random), the
  `TARTINE_IOC_MAKE_WRITABLE` conversion ioctl (§9.3 step 3 needs to
  stream-read the very chunk-log records that are gone), and `ftruncate`
  to a nonzero size (would need to preserve surviving bytes across the
  resize). `EIO` rather than a bespoke errno: this is exactly what a
  conventional filesystem reports for an unreadable sector, and `tartine`
  doesn't need a special vocabulary for "your data is unrecoverable" that
  application code wouldn't already know how to handle.

**Granularity is whole-file, not per-chunk/per-extent**, even though a
file with `replication_factor > 1` could in principle lose only some of
its chunks while others remain perfectly readable. This is a deliberate
v1 simplification, in the same spirit as §11's fixed extent size: POSIX
`read()` has no good way to say "bytes 0–4095 are fine, 4096–8191 are
gone, keep going" without every caller having to learn a new convention,
and once *any* part of a file is definitively unrecoverable, `rm` +
restore-from-wherever-the-real-backup-is is the practical recovery path
regardless of how much of the rest survived. A future refinement could
track loss per chunk/extent and only fail reads that actually touch a
lost range — noted as an option in §14, not built here.

## 9. Append-only files and the writable conversion

### 9.1 Default state: `AppendOnly`

Every newly created file starts with `InodeRecord.mode = AppendOnly`. In
this state:

- `write()` is only accepted at the current end-of-file offset — enforced
  by the filesystem regardless of the flags the client opened with (i.e.
  even without `O_APPEND`, non-append writes are rejected), matching the
  "append-only at birth" requirement rather than relying on the calling
  process to behave. This is `tartine_kcore::write_path::classify_write`
  (§4.1/§4.2) — the same function decides this for both the kernel module
  and the FUSE prototype.
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

Three equivalent entry points, all routed to the same underlying
operation — `kernel/tartine.h` and `crates/tartine-fuse/src/ioctl.rs`
define the identical numbers, so the same `ioctl(2)` call works
unchanged whether the mount is the kernel module or the FUSE prototype:

```c
/* kernel/tartine.h */
#define TARTINE_IOC_MAGIC        'T'
#define TARTINE_IOC_MAKE_WRITABLE   _IOW(TARTINE_IOC_MAGIC, 1, __u32 /* flags */)
#define TARTINE_IOC_GET_STATE       _IOR(TARTINE_IOC_MAGIC, 2, struct tartine_state)

struct tartine_state {
    __u32 mode;        /* TARTINE_MODE_APPEND_ONLY | _CONVERTING | _WRITABLE */
    __u32 _pad;        /* explicit — see below */
    __u64 bytes_total;
    __u64 bytes_converted;   /* progress while _CONVERTING */
};
```

- `ioctl(fd, TARTINE_IOC_MAKE_WRITABLE, &flags)` — `flags` bit 0 selects
  synchronous (blocks the ioctl until conversion is fully complete) vs
  asynchronous (returns immediately, state becomes `Converting`, caller
  polls `TARTINE_IOC_GET_STATE`). `.unlocked_ioctl` in the kernel module
  and FUSE's `ioctl` op in the prototype both map straightforwardly to
  this — `kernel/tartine_main.c`'s `tartine_file_ioctl()` is the current
  (partial — see `kernel/README.md`) implementation.
- `setxattr(path, "user.tartine.writable", "1", ...)` — convenience for
  tools that can't easily issue raw ioctls (shell scripts via
  `setfattr`); same async behavior as the default ioctl.
- `tartinectl convert <path> [--wait]` — CLI wrapper issuing the same
  ioctl, useful for batch/offline conversion and for scripting.

Two ABI/permission details that are cheap now and expensive to retrofit:

- **Explicit padding**: `_pad` above is not decorative. Without it,
  64-bit compilers insert 4 invisible bytes before `bytes_total` and
  32-bit compilers don't, so `sizeof(struct tartine_state)` — which the
  `_IOR` macro encodes into the command number — differs between 32- and
  64-bit userspace, and one of the two gets `-ENOTTY`. With all fields
  fixed-width and padding explicit, `.compat_ioctl = compat_ptr_ioctl`
  is the only 32-bit-compat plumbing needed. Both definitions
  (`kernel/tartine.h`, `crates/tartine-fuse/src/ioctl.rs`) carry
  compile-time size asserts so drift fails the build.
- **Permission**: conversion changes write semantics for every open fd
  on the file, so it's restricted like `chmod`: file owner or
  `CAP_FOWNER` (`inode_owner_or_capable()` in the kernel module), not
  merely "can open for write".

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
   snapshot; the materializer picks them up in delta passes) — so a
   long-running append workload isn't blocked by a slow conversion.
   Delta passes are **bounded**, or a sustained append workload livelocks
   the conversion (each pass ends with new appends already outstanding):
   after at most k passes (default 3), or earlier once a pass's remaining
   delta is under a small threshold, the materializer briefly quiesces
   appends — new appends block for the milliseconds it takes to copy the
   final tail — then performs the swap and unblocks. Conversion therefore
   always terminates, at the cost of one short, bounded append stall.
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

## 10. Per-file redundancy policy

A file's redundancy isn't just a replica *count* — it's a list of
replica *slots*, each independently constrained. This is what makes all
of the following expressible as the same mechanism instead of special
cases bolted onto each other:

- "unreplicated, on SSD" — one slot, class = SSD.
- "unreplicated, on this particular disk" — one slot, pinned to a disk.
- "replicated 3 times, whichever disks" — three slots, no constraint.
- "one HDD and one SSD, so reads are SSD-fast" — two slots, classes
  HDD and SSD.

```rust
enum DiskClass { Hdd, Ssd, Nvme }

enum ReplicaSlot {
    AnyOfClass(Option<DiskClass>),  // None = no constraint (today's default)
    Pinned(DiskId),
}

enum RedundancyScheme {
    Replicated(Vec<ReplicaSlot>),
    /// Reserved, not implemented — §16.6.
    ErasureCoded { data_shards: u8, parity_shards: u8 },
}
```

`InodeRecord.redundancy: RedundancyScheme` replaces the earlier scalar
`replication_factor: u8`. `RedundancyScheme::replica_count()` is what
that scalar becomes when only the count matters (rebalancer bookkeeping,
`statfs` estimates); `Replicated(slots).len() == N` recovers the old
"replicated N times" behavior exactly, since `AnyOfClass(None)` for
every slot places identically to unconstrained HRW (verified by
`tartine-kcore`'s `place_redundancy_matches_hrw_select_for_uniform_any_slots`
test — the general mechanism reproduces the simple case bit-for-bit, not
just approximately).

### 10.1 Disk classes

`DiskEntry.class: DiskClass` is set at `disk add` time: auto-detected
from the block device's rotational flag (`blk_queue_is_rotational()` —
distinguishes HDD from everything else) with a manual override
(`tartinectl disk add /dev/nvme1n1 --class nvme`), since auto-detection
alone can't tell NVMe from SATA SSD and is sometimes wrong for
virtual/passthrough devices. Three classes today (HDD/SSD/NVMe) rather
than open-ended operator-defined tags (the way Ceph CRUSH device classes
work) — deliberately: a fixed small enum is what lets
`DiskCandidate::class` stay a plain `u8` across the kernel FFI boundary
with no string interning. If per-deployment tags turn out to be needed,
the natural extension is to intern them pool-wide into small integers at
`disk add` time and keep the wire format exactly as-is — noted here as a
clean extension point, not designed further since nothing in the current
requirements needs it.

### 10.2 Placement: one slot at a time, excluding what's already placed

`tartine-kcore::placement::place_redundancy` (DESIGN.md §7.2's HRW,
generalized) walks a file's slot list in order. For each slot: if it's
pinned, the named disk is used directly (no HRW — it's a lookup, not a
score); otherwise HRW selects the highest-scoring `Active` disk of the
required class, excluding every disk already chosen for an earlier slot
of the *same* file, so two slots can never collide on one disk. This is
exactly the exclusion loop `hrw_select`'s multi-replica case always used
internally — §7.2's "any disk" replication was already a special case of
this, it just didn't have a name for the general form until per-slot
constraints made one necessary.

An unsatisfiable slot (no HDD in the pool when one was requested; the
pinned disk is gone) reports that slot as unplaced rather than failing
the whole call — DESIGN.md §7.4's "under-replicated, repair when
possible" framing applies per-slot, so one bad constraint doesn't block
the other slots of the same file from being placed. What differs is what
happens *next*: at policy-set time (§10.3) an unsatisfiable slot is a
hard error (the operator asked for something the pool can't currently
provide, and silently accepting it would be misleading); for an
already-placed file whose class later runs out of room, it's ordinary
under-replication that the repair loop (§8) retries.

**Pinning's tradeoff, stated plainly**: a pinned slot has no fallback by
construction. If the pinned disk is gone, that replica is gone — there
is no "pin failed, pick another disk" behavior, because that would
silently defeat the reason to pin something (a locality/latency
guarantee, e.g. "this replica must be on the disk attached to this GPU
node") in favor of a resilience guarantee the operator didn't ask this
particular slot to provide. An operator who wants both locality *and*
a fallback expresses that as two slots: one pinned, one
class-constrained or unconstrained.

### 10.3 Setting and changing the policy

```
tartinectl file set-redundancy <path> <spec>
tartinectl file get-redundancy <path>
tartinectl fs set-default-redundancy <spec>   # applied to newly created files
# or: setfattr -n user.tartine.redundancy -v <spec> <path>
```

`<spec>` is the compact grammar `crates/tartine-core/src/redundancy_spec.rs`
implements and tests: a bare integer (`3`) for N unconstrained replicas;
otherwise a comma-separated list of slots, each `any` / `hdd` / `ssd` /
`nvme` / `disk:<uuid>` (`ssd`, `disk:<uuid>`, `hdd,ssd`); or `rs:<k>+<m>`
for the reserved erasure-coding syntax (§16.6). `tartinectl` parses this
client-side and issues the *structured* ioctl
(`TARTINE_IOC_SET_REDUNDANCY`, `kernel/tartine.h`) — the kernel module
never parses this grammar for the ioctl path, only (eventually) for the
`setxattr(2)` convenience path, which needs its own small hand-written
parser since it can't call into userspace Rust.

Changing the policy doesn't move data synchronously — same as the
original scalar design, generalized: it updates the target
`RedundancyScheme` in metadata and lets the rebalancer (§7.3's mechanism,
unchanged) converge actual placement to it in the background, honoring
whatever slot constraints changed (add replicas by placing new slots;
remove excess by deleting whichever placed disks are no longer named by
any slot; move a class-constrained replica if its disk's class no longer
matches; move a pinned replica if the pin target changed). One mechanism
still covers "pool topology changed", "this file's policy changed", and
now "this file's per-slot constraints changed" — the rebalancer's job
was already "converge actual placement to target placement," and slots
didn't change what that job is, only what "target" can express.

## 11. Data path summary

Written from the kernel module's point of view (`write_iter`/`read_iter`
on `struct file_operations`); the FUSE prototype's `write`/`read` handlers
follow the identical shape one layer up.

**Append write** (`AppendOnly` file): `write_iter` → `tartine_classify_write`
validates offset == EOF and mode (§4.1, real today — see
`kernel/tartine_main.c`) → HRW (`tartine_hrw_select`) picks N data disks
for `(inode, chunk_seq)` → parallel `bio` writes to all N (fan-out, wait
per configured consistency: `all` by default, `quorum` optional for lower
tail latency at reduced durability) → on success, metadata txn appends
the chunk pointer (§6, synchronous 2-disk WAL) → ack to caller. **Not
implemented yet**: everything from "HRW picks N data disks" onward —
`kernel/tartine_main.c`'s `write_iter` currently returns `-EOPNOTSUPP`
once classification passes (`kernel/README.md`).

**Read**: `read_iter` → resolve offset to chunk-log record(s) or extent(s)
from metadata (in-memory cache, backed by the metadata B-tree) → pick one
live replica (prefer local/least-loaded disk) → `bio` read + checksum
verify → on checksum failure, retry next replica + kick repair. **Not
implemented yet** — no `read_iter` at all in the current skeleton.

**Random write** (`Writable` file only): `write_iter` at arbitrary offset →
resolve/allocate extents → read-modify-write if sub-block, else direct
overwrite → fan-out to replica set → metadata txn only needed if the
extent map changed (new allocation, size growth), not per write.

## 12. Failure & recovery matrix

| Event | Detection | Response |
|---|---|---|
| Data disk disappears | I/O error / device removal uevent | Mark `Dead` in pool map (metadata txn); chunks/extents it held are under-replicated → repair loop re-replicates from survivors to newly HRW-selected disks (§8.1). |
| Data disk disappears *and* also held a metadata replica (§5.1 — common on small pools, always true for a single-disk pool) | Same detection as both single-role rows, same event | Metadata resync (row below) is serviced first; the data repair above for that disk's chunks is queued behind it, not skipped (§8.2). |
| Metadata disk disappears | I/O error on WAL write | Fence out of `MetaGroup`, `epoch += 1`, continue degraded (primary-only) while provisioning + resyncing a replacement (§6). |
| Both metadata disks gone simultaneously | Mount fails | Manual recovery: operator points a mount attempt at any surviving data disks; namespace is unrecoverable beyond what can be reconstructed from data-disk superblocks' cached `MetaGroup`/pool-map echoes (best-effort) — this is the one true single point of failure in v1, called out explicitly in §14. |
| A chunk/extent's last surviving replica is lost (a `replication_factor == 1` file's only disk dies, e.g. any file on a single-disk pool; or simultaneous loss exceeds a file's configured replication factor) | Repair loop finds zero surviving replicas for the chunk/extent | Inode's `data_lost` flag is set (§8.3); `read`/`write`/`TARTINE_IOC_MAKE_WRITABLE`/nonzero-`ftruncate` return `EIO`; `unlink`/`chmod`/`chown`/`stat` still work; logged for operator visibility instead of being retried forever. |
| Power loss / crash mid-write | On next mount, WAL replay | Data disks: incomplete trailing record in a segment is detected via checksum/length sanity and truncated (standard log-structured recovery). Metadata: WAL is replayed from last checkpoint on both metadata disks (they agree, since writes were synchronous); any op whose data-write never got acked is simply absent from the WAL and never happened. |
| Bit rot | Scrubber / read-time checksum mismatch | Repair from healthy replica (§8.1). |
| Conversion interrupted (crash mid-`Converting`) | Inode `mode == Converting` found on restart | Discard partial extent map, resume from step 2 of §9.3 (old chunk-log is still intact and authoritative until the atomic swap in step 5, so this is always safe to restart from scratch). |

## 13. Explicit non-goal: multi-host HA

Everything above assumes one host's mounted kernel module owns the pool.
If the requirement ever grows to "survive losing the whole host", the
natural extension is active/standby mounts on two hosts sharing access to
the same disks (multi-initiator NVMe-oF/iSCSI, or simply each disk being
dual-ported), with leader election (now genuinely needing distributed
consensus — out of scope for an in-kernel component, more realistically a
small userspace arbiter the module defers to) over *who is allowed to
write* — at that point the "2 disks for metadata" constraint would need
to be revisited alongside "2 hosts for the writer role" as a related but
distinct decision. Called out here so the line between what's designed
and what's future work is explicit.

## 14. Known limitations / open questions

- **A single mount is a SPOF for availability** (not for durability — data
  survives; the pool just can't be *served* from a second host while the
  mounting host is down). Acceptable for v1's stated scope (single host);
  §13 is the extension path.
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
- No mandatory locking / range locking specified — the VFS's standard
  `flock`/`fcntl` advisory-lock plumbing applies to any kernel filesystem
  more or less for free, but per-file-replica-aware semantics (if any are
  even needed, given the single-writer model of §2) aren't designed here.
- The kernel module (`kernel/`) is, today, VFS/kbuild plumbing plus the
  ioctl-driven state machine — not yet a working filesystem. §15 and
  `kernel/README.md` are explicit about exactly what's missing (block
  I/O, the on-disk metadata B-tree, the rebalancer/scrubber) and why it
  wasn't attempted further without a real kernel build tree to verify
  against.
- `data_lost` (§8.3) is a whole-file flag, not per-chunk/per-extent, so a
  file that lost only one chunk out of many is quarantined exactly the
  same as one that lost everything. Tracking loss at chunk/extent
  granularity and only failing reads that actually touch a lost range is
  a plausible future refinement, not built here.

## 15. Suggested milestones

**Phase 1 — prototype in FUSE** (validate the design where mistakes are cheap):

1. ✅ `tartine-kcore`: HRW placement and the append-only/converting/writable
   state machine, `#![no_std]`, zero dependencies, unit-tested — the one
   implementation both phases below end up using.
2. ✅ `tartine-core`: disk superblock format, chunk-log segment read/write
   (own implementation of the on-disk record format tartine-kcore doesn't
   own — §5.3), plus a thin `Vec`/`PoolMap` wrapper around
   `tartine-kcore`'s placement.
3. `tartine-meta`: `redb`-backed inode table + directory tree + WAL, single
   in-process metadata group (no replication yet) — enough for a
   single-disk, non-replicated prototype. *(scaffolded, not implemented)*
4. ✅ `tartine-fuse`: write-path state machine and ioctl surface wired to
   `tartine-kcore`, unit-tested. *(FUSE session itself — actually mounting
   and serving I/O — not implemented yet: `tartined`'s `main.rs` is still
   a stub.)*
5. Add the second metadata disk + synchronous WAL shipping (§6) to the
   prototype.
6. Multi-disk pool + HRW-driven placement + `tartinectl disk add/remove`
   (§7), per-file replication factor + rebalancer (§10), scrubber/repair
   (§8, §12) — all in the FUSE prototype first.

**Phase 2 — port the validated design to `kernel/`** (production):

7. ✅ Module skeleton: registration, `fs_context`-based mount, superblock
   read+checksum+parse, minimal root inode, `tartine-kcore` linked in and
   driving the ioctl/write-classification surface. *(Written, not
   build-verified — no kernel headers were available where this was
   authored; see `kernel/README.md`.)*
8. Get it actually compiling and loading against a real kernel build
   tree; fix the block-device-open API and any kbuild link issues
   `kernel/README.md` flags as unverified.
9. The on-disk metadata B-tree + 2-disk WAL replication (§6), rebuilt
   against the block layer directly — this is the one piece Phase 1
   deliberately didn't validate a kernel-shaped implementation of (it used
   `redb`), so budget real design time here, not just porting time.
10. Chunk-log/extent block I/O via `bio`/`blk-mq` (§5.3, §11) — the
    `write_iter`/`read_iter` gap `kernel/README.md` calls out.
11. Multi-disk pool, device-scan registry, rebalancer, scrubber (§7, §8) —
    now driven by ioctls on the mount / control device instead of a
    prototype daemon's in-process logic.
12. The `ioctl` conversion path's actual materializer (§9.3 steps 2-5) —
    last, since everything above needs to be solid before "costly,
    one-way, background materialization" is safe to build on.
13. Kernel-safe codegen for `tartine-kcore`'s `freestanding` build (no red
    zone, kernel code model, verified against a real link — see
    `kernel/README.md`'s "Making the Rust object actually kernel-safe").
14. Golden on-disk-format test vectors shared between the FUSE prototype
    and the kernel module's test suite, closing the gap flagged in §5.3
    (two independent implementations of the same record layout, not
    provably in agreement today beyond "both follow this document").

## 16. Reviewed proposals (agreed direction, not yet scheduled)

Design-review outcomes that change architecture rather than fix defects
(defects found in the same review were fixed in place: the weighted-HRW
bias in §7.2, fence durability ordering in §6, bounded conversion delta
passes in §9.3, the ioctl ABI/permission rules in §9.2). Each of these is
believed right but deliberately not yet folded into the main design text,
because each changes something Phase 1 (§15) should validate first.

### 16.1 Self-describing chunk-log; demote the chunk index to a rebuildable cache

Today every append pays a synchronous 2-disk metadata WAL commit for its
chunk pointer — two extra device flushes on the hottest path in the
system. But each log record already carries `(inode, chunk_seq, len,
checksum)` (§5.3): the segments themselves can be the authority, exactly
as Bitcask treats its data files. The chunk-pointer list in the metadata
store then becomes a lazily-maintained index, checkpointed periodically
and rebuilt after a crash by scanning segment tails written since the
last checkpoint. The synchronous 2-disk WAL shrinks to what actually
needs it: namespace operations (create/rename/unlink), pool-map and
meta-group changes, mode transitions — all rare. `fsync(fd)` on an
append-only file then means "flush the file's outstanding segment
records on all N replicas", with no metadata flush required for
durability of data. This is the single biggest write-latency lever
available and *simplifies* the recovery story (one authority instead of
two that must agree). Cost: recovery-time tail scans, and the index must
tolerate being stale — both well-trodden log-structured territory.

### 16.2 Placement-group indirection

HRW keyed per `(inode, chunk_seq)` with replica lists stored per chunk
means a disk add/remove rewrites metadata for every moved chunk —
O(moved chunks) metadata transactions of churn. Interposing a fixed set
of placement groups (`PG = hash(inode, chunk_seq) mod 2^14`; a small
`PG → [DiskId; N]` table as one metadata row) makes topology changes
update table entries instead of per-chunk pointers, gives repair and
scrub a natural whole-PG unit of work, and bounds what the rebalancer
must track. This is Ceph's PG idea shrunk to single-host scale. It
composes with 16.1 (records stay self-describing; only the *routing*
gains a level of indirection) and partially supersedes per-chunk replica
lists — which is exactly why it should land before any on-disk format is
declared stable.

### 16.3 Page-cache integration for reads

The design currently routes all I/O through direct block access. Right
for replication fan-out writes; wrong as the only read path — a kernel
filesystem gets caching, readahead, and read-only `mmap` nearly free via
`address_space_operations`, and append-only files are the best possible
tenant for the page cache (immutable-once-written pages never need
invalidating). Without this, cold small reads will benchmark
embarrassingly against ext4 and `grep`/`mmap`-heavy workloads won't work
at all. Plan: reads through the page cache in both file modes;
`O_DIRECT` honored when requested; replication writes keep the direct
path.

### 16.4 Crash-consistency contract and mechanical crash testing

Write down what `fsync`, `O_SYNC`, and `close` promise per file mode
(with 16.1: data durability = segment records flushed on all N replicas;
namespace durability = WAL commit), then enforce it mechanically rather
than by review: `dm-log-writes` replay to test every write-boundary
crash point, `dm-flakey` for fault injection during degraded/fence
transitions (§6's ordering rules are exactly the kind of thing only this
style of testing catches), and xfstests wired up as soon as the FUSE
prototype can mount — the generic suite finds VFS-contract violations
that unit tests structurally cannot.

### 16.5 Snapshots for append-only files

With immutable chunk-log records (and especially with 16.1), a snapshot
of an append-only file is a copy of its pointer list plus a refcount on
the records — no data copy, no CoW machinery beyond what the compactor
already respects. Writable files would need real CoW extents and are
explicitly *not* included. Worth doing early only because it's nearly
free on this layout and pins down record-refcounting semantics the
compactor needs anyway; otherwise it stays future work.

### 16.6 Erasure coding (Reed-Solomon) — reserved, not implemented

Requested direction, not yet designed in depth: `RedundancyScheme::ErasureCoded
{ data_shards, parity_shards }` (§10) reserves the type and wire-format
space (`kernel/tartine.h`'s `TARTINE_REDUNDANCY_ERASURE_CODED`,
`crates/tartine-core/src/redundancy_spec.rs`'s `"rs:k+m"` syntax) so
adopting it later doesn't force an on-disk or ioctl format break. Every
placement/read/write/repair path rejects it today
(`PlaceError::ErasureCodedUnimplemented` in `tartine-core`,
`-EOPNOTSUPP` in `kernel/tartine_main.c`).

What actually implementing it would need, sketched at the depth worth
recording now without committing to specifics that should be decided
against real workload data:

- **Placement**: a `(k, m)` stripe spans `k + m` disks chosen the same
  way a replicated file's slots are (`place_redundancy`, one slot per
  shard, no class/pin constraint needed for parity shards specifically
  but not precluded either) — the placement *mechanism* generalizes for
  free; what's new is shard *encoding*, not shard *placement*.
- **Write path**: erasure coding is fundamentally incompatible with the
  append-only chunk-log's per-append independence (§9.1) — computing
  parity needs a full stripe's worth of data, not one append at a time.
  The natural fit is at the append→writable conversion boundary (§9.3):
  materialize into EC stripes instead of plain extents when the target
  scheme is `ErasureCoded`, meaning EC files are implicitly writable-only
  in practice even though the type doesn't force that. Append-only files
  requesting EC would need either buffering appends until a full stripe
  accumulates (latency cost) or a smaller-than-optimal partial-stripe
  encoding for the tail (space cost) — an open question, not resolved
  here.
- **Read path**: reconstruct from `k` of the `k + m` shards on a missing
  shard, same cost/complexity class as replica-set fallback (§8) but
  with actual RS math instead of "read the other copy."
- **Repair**: losing a disk that held a shard means a full-stripe
  reconstruction (read `k` shards, recompute, write the replacement) —
  meaningfully more expensive per-lost-shard than replicated repair
  (§8's "copy from a surviving replica"), which is the standard EC
  resilience/repair-cost tradeoff, not specific to this design.
- **Kernel constraints**: RS arithmetic is GF(2^8) polynomial math —
  no floating point (already a constraint everywhere else in this
  design, §4.1) and ideally using `CONFIG_RAID6_PQ`'s existing kernel
  GF(2^8) routines (already present for `md` RAID6) rather than a new
  from-scratch implementation, if the algebra lines up — worth checking
  before writing anything, not assumed here.

None of this is scheduled; it's recorded so the reservation in §10's
types is traceable to an actual plan rather than a placeholder with no
follow-through.
