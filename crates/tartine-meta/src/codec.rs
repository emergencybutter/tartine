//! Hand-rolled binary encoding for `MetaOp` (and everything it embeds:
//! `InodeRecord`, `RedundancyScheme`, `PoolMap`, ...) — what gets framed
//! and written to the WAL (`wal.rs`) and what gets stored as the value
//! bytes in `store.rs`'s redb tables.
//!
//! No `serde`/`bincode`: the format is simple enough (fixed-width
//! integers, length-prefixed vecs/strings/options) that hand-writing it
//! is barely more code than deriving it, and it keeps this crate's
//! on-the-wire format fully explicit and auditable in one place, the
//! same choice `tartine-core::segment` already made for the chunk-log
//! record format.
//!
//! `Decoder` panics on malformed input rather than returning `Result`
//! for every primitive read. That's a real, deliberate scope limit for
//! the prototype: these bytes only ever come from a WAL this same code
//! wrote (or from redb, which has its own integrity guarantees), so a
//! malformed decode indicates a bug in this codec, not adversarial or
//! corrupted input reaching it — corruption at rest is `wal.rs`'s/
//! `segment.rs`'s checksum's job to catch *before* a payload ever
//! reaches `Decoder`, not this layer's.

use tartine_proto::{
    ChunkPointer, DataLocator, DiskClass, DiskEntry, DiskRoles, DiskState, Extent, InodeId,
    InodeMode, InodeRecord, MetaGroup, MetaOp, PoolMap, RedundancyScheme, ReplicaSlot, Uuid,
};

pub struct Encoder(Vec<u8>);

impl Encoder {
    pub fn new() -> Self {
        Encoder(Vec::new())
    }
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn bool(&mut self, v: bool) {
        self.u8(v as u8);
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u128(&mut self, v: u128) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Decoder { buf, pos: 0 }
    }
    pub fn u8(&mut self) -> u8 {
        let v = self.buf[self.pos];
        self.pos += 1;
        v
    }
    pub fn bool(&mut self) -> bool {
        self.u8() != 0
    }
    pub fn u32(&mut self) -> u32 {
        let v = u32::from_le_bytes(self.buf[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        v
    }
    pub fn u64(&mut self) -> u64 {
        let v = u64::from_le_bytes(self.buf[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        v
    }
    pub fn u128(&mut self) -> u128 {
        let v = u128::from_le_bytes(self.buf[self.pos..self.pos + 16].try_into().unwrap());
        self.pos += 16;
        v
    }
    pub fn f64(&mut self) -> f64 {
        let v = f64::from_le_bytes(self.buf[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        v
    }
    pub fn bytes(&mut self) -> Vec<u8> {
        let len = self.u32() as usize;
        let v = self.buf[self.pos..self.pos + len].to_vec();
        self.pos += len;
        v
    }
    pub fn str(&mut self) -> String {
        String::from_utf8(self.bytes()).expect("codec only ever decodes strings it encoded")
    }
}

fn encode_uuid(e: &mut Encoder, id: Uuid) {
    e.u128(id.0);
}
fn decode_uuid(d: &mut Decoder) -> Uuid {
    Uuid(d.u128())
}

fn encode_disk_class(e: &mut Encoder, c: DiskClass) {
    e.u8(match c {
        DiskClass::Hdd => 0,
        DiskClass::Ssd => 1,
        DiskClass::Nvme => 2,
    });
}
fn decode_disk_class(d: &mut Decoder) -> DiskClass {
    match d.u8() {
        0 => DiskClass::Hdd,
        1 => DiskClass::Ssd,
        2 => DiskClass::Nvme,
        other => panic!("bad DiskClass tag {other}"),
    }
}

fn encode_replica_slot(e: &mut Encoder, slot: &ReplicaSlot) {
    match slot {
        ReplicaSlot::AnyOfClass(None) => e.u8(0),
        ReplicaSlot::AnyOfClass(Some(c)) => {
            e.u8(1);
            encode_disk_class(e, *c);
        }
        ReplicaSlot::Pinned(id) => {
            e.u8(2);
            encode_uuid(e, *id);
        }
    }
}
fn decode_replica_slot(d: &mut Decoder) -> ReplicaSlot {
    match d.u8() {
        0 => ReplicaSlot::AnyOfClass(None),
        1 => ReplicaSlot::AnyOfClass(Some(decode_disk_class(d))),
        2 => ReplicaSlot::Pinned(decode_uuid(d)),
        other => panic!("bad ReplicaSlot tag {other}"),
    }
}

fn encode_redundancy_scheme(e: &mut Encoder, s: &RedundancyScheme) {
    match s {
        RedundancyScheme::Replicated(slots) => {
            e.u8(0);
            e.u32(slots.len() as u32);
            for slot in slots {
                encode_replica_slot(e, slot);
            }
        }
        RedundancyScheme::ErasureCoded {
            data_shards,
            parity_shards,
        } => {
            e.u8(1);
            e.u8(*data_shards);
            e.u8(*parity_shards);
        }
    }
}
fn decode_redundancy_scheme(d: &mut Decoder) -> RedundancyScheme {
    match d.u8() {
        0 => {
            let n = d.u32();
            RedundancyScheme::Replicated((0..n).map(|_| decode_replica_slot(d)).collect())
        }
        1 => RedundancyScheme::ErasureCoded {
            data_shards: d.u8(),
            parity_shards: d.u8(),
        },
        other => panic!("bad RedundancyScheme tag {other}"),
    }
}

fn encode_inode_mode(e: &mut Encoder, m: InodeMode) {
    e.u8(match m {
        InodeMode::AppendOnly => 0,
        InodeMode::Converting => 1,
        InodeMode::Writable => 2,
    });
}
fn decode_inode_mode(d: &mut Decoder) -> InodeMode {
    match d.u8() {
        0 => InodeMode::AppendOnly,
        1 => InodeMode::Converting,
        2 => InodeMode::Writable,
        other => panic!("bad InodeMode tag {other}"),
    }
}

fn encode_replica_offsets(e: &mut Encoder, replicas: &[(Uuid, u64)]) {
    e.u32(replicas.len() as u32);
    for (id, offset) in replicas {
        encode_uuid(e, *id);
        e.u64(*offset);
    }
}
fn decode_replica_offsets(d: &mut Decoder) -> Vec<(Uuid, u64)> {
    let n = d.u32();
    (0..n).map(|_| (decode_uuid(d), d.u64())).collect()
}

fn encode_chunk_pointer(e: &mut Encoder, c: &ChunkPointer) {
    e.u64(c.chunk_seq);
    encode_replica_offsets(e, &c.replicas);
    e.u32(c.len);
    e.u32(c.checksum);
}
fn decode_chunk_pointer(d: &mut Decoder) -> ChunkPointer {
    let chunk_seq = d.u64();
    let replicas = decode_replica_offsets(d);
    let len = d.u32();
    let checksum = d.u32();
    ChunkPointer {
        chunk_seq,
        replicas,
        len,
        checksum,
    }
}

pub fn encode_extent(e: &mut Encoder, x: &Extent) {
    e.u64(x.file_offset);
    e.u32(x.len);
    encode_replica_offsets(e, &x.replicas);
    e.u32(x.checksum);
}
pub fn decode_extent(d: &mut Decoder) -> Extent {
    let file_offset = d.u64();
    let len = d.u32();
    let replicas = decode_replica_offsets(d);
    let checksum = d.u32();
    Extent {
        file_offset,
        len,
        replicas,
        checksum,
    }
}

fn encode_data_locator(e: &mut Encoder, dl: &DataLocator) {
    match dl {
        DataLocator::ChunkLog(v) => {
            e.u8(0);
            e.u32(v.len() as u32);
            for c in v {
                encode_chunk_pointer(e, c);
            }
        }
        DataLocator::Extents(v) => {
            e.u8(1);
            e.u32(v.len() as u32);
            for x in v {
                encode_extent(e, x);
            }
        }
    }
}
fn decode_data_locator(d: &mut Decoder) -> DataLocator {
    match d.u8() {
        0 => {
            let n = d.u32();
            DataLocator::ChunkLog((0..n).map(|_| decode_chunk_pointer(d)).collect())
        }
        1 => {
            let n = d.u32();
            DataLocator::Extents((0..n).map(|_| decode_extent(d)).collect())
        }
        other => panic!("bad DataLocator tag {other}"),
    }
}

pub fn encode_inode_record(e: &mut Encoder, r: &InodeRecord) {
    e.u64(r.inode);
    encode_inode_mode(e, r.mode);
    e.u64(r.size);
    encode_redundancy_scheme(e, &r.redundancy);
    encode_data_locator(e, &r.data);
    e.u32(r.uid);
    e.u32(r.gid);
    e.u32(r.unix_mode);
    e.u64(r.mtime_unix);
}
pub fn decode_inode_record(d: &mut Decoder) -> InodeRecord {
    let inode = d.u64();
    let mode = decode_inode_mode(d);
    let size = d.u64();
    let redundancy = decode_redundancy_scheme(d);
    let data = decode_data_locator(d);
    let uid = d.u32();
    let gid = d.u32();
    let unix_mode = d.u32();
    let mtime_unix = d.u64();
    InodeRecord {
        inode,
        mode,
        size,
        redundancy,
        data,
        uid,
        gid,
        unix_mode,
        mtime_unix,
    }
}

fn encode_disk_roles(e: &mut Encoder, r: DiskRoles) {
    e.u8((r.data as u8) | ((r.metadata as u8) << 1));
}
fn decode_disk_roles(d: &mut Decoder) -> DiskRoles {
    let b = d.u8();
    DiskRoles {
        data: b & 1 != 0,
        metadata: b & 2 != 0,
    }
}

fn encode_disk_state(e: &mut Encoder, s: DiskState) {
    e.u8(match s {
        DiskState::Active => 0,
        DiskState::Draining => 1,
        DiskState::Dead => 2,
    });
}
fn decode_disk_state(d: &mut Decoder) -> DiskState {
    match d.u8() {
        0 => DiskState::Active,
        1 => DiskState::Draining,
        2 => DiskState::Dead,
        other => panic!("bad DiskState tag {other}"),
    }
}

fn encode_disk_entry(e: &mut Encoder, entry: &DiskEntry) {
    encode_disk_roles(e, entry.roles);
    encode_disk_state(e, entry.state);
    encode_disk_class(e, entry.class);
    e.f64(entry.weight);
    e.u64(entry.used_bytes);
    e.u64(entry.capacity_bytes);
}
fn decode_disk_entry(d: &mut Decoder) -> DiskEntry {
    let roles = decode_disk_roles(d);
    let state = decode_disk_state(d);
    let class = decode_disk_class(d);
    let weight = d.f64();
    let used_bytes = d.u64();
    let capacity_bytes = d.u64();
    DiskEntry {
        roles,
        state,
        class,
        weight,
        used_bytes,
        capacity_bytes,
    }
}

pub fn encode_pool_map(e: &mut Encoder, pm: &PoolMap) {
    e.u64(pm.epoch);
    e.u32(pm.disks.len() as u32);
    for (id, entry) in &pm.disks {
        encode_uuid(e, *id);
        encode_disk_entry(e, entry);
    }
}
pub fn decode_pool_map(d: &mut Decoder) -> PoolMap {
    let epoch = d.u64();
    let n = d.u32();
    let mut disks = std::collections::HashMap::with_capacity(n as usize);
    for _ in 0..n {
        let id = decode_uuid(d);
        let entry = decode_disk_entry(d);
        disks.insert(id, entry);
    }
    PoolMap { epoch, disks }
}

pub fn encode_meta_group(e: &mut Encoder, g: &MetaGroup) {
    encode_uuid(e, g.primary);
    e.bool(g.backup.is_some());
    if let Some(backup) = g.backup {
        encode_uuid(e, backup);
    }
    e.u64(g.epoch);
}
pub fn decode_meta_group(d: &mut Decoder) -> MetaGroup {
    let primary = decode_uuid(d);
    let has_backup = d.bool();
    let backup = if has_backup {
        Some(decode_uuid(d))
    } else {
        None
    };
    let epoch = d.u64();
    MetaGroup {
        primary,
        backup,
        epoch,
    }
}

pub fn encode_op(op: &MetaOp) -> Vec<u8> {
    let mut e = Encoder::new();
    match op {
        MetaOp::CreateInode(rec) => {
            e.u8(0);
            encode_inode_record(&mut e, rec);
        }
        MetaOp::Link {
            parent,
            name,
            child,
        } => {
            e.u8(1);
            e.u64(*parent);
            e.str(name);
            e.u64(*child);
        }
        MetaOp::Unlink { parent, name } => {
            e.u8(2);
            e.u64(*parent);
            e.str(name);
        }
        MetaOp::AppendChunk { inode, chunk } => {
            e.u8(3);
            e.u64(*inode);
            encode_chunk_pointer(&mut e, chunk);
        }
        MetaOp::BeginConvert { inode } => {
            e.u8(4);
            e.u64(*inode);
        }
        MetaOp::CompleteConvert { inode, extents } => {
            e.u8(5);
            e.u64(*inode);
            e.u32(extents.len() as u32);
            for x in extents {
                encode_extent(&mut e, x);
            }
        }
        MetaOp::SetRedundancyScheme { inode, scheme } => {
            e.u8(6);
            e.u64(*inode);
            encode_redundancy_scheme(&mut e, scheme);
        }
        MetaOp::PoolMapChange(pm) => {
            e.u8(7);
            encode_pool_map(&mut e, pm);
        }
        MetaOp::MetaGroupChange(mg) => {
            e.u8(8);
            encode_meta_group(&mut e, mg);
        }
        MetaOp::UpdateExtents { inode, extents } => {
            e.u8(9);
            e.u64(*inode);
            e.u32(extents.len() as u32);
            for x in extents {
                encode_extent(&mut e, x);
            }
        }
    }
    e.into_vec()
}

pub fn decode_op(bytes: &[u8]) -> MetaOp {
    let mut d = Decoder::new(bytes);
    match d.u8() {
        0 => MetaOp::CreateInode(decode_inode_record(&mut d)),
        1 => {
            let parent = d.u64();
            let name = d.str();
            let child = d.u64();
            MetaOp::Link {
                parent,
                name,
                child,
            }
        }
        2 => {
            let parent = d.u64();
            let name = d.str();
            MetaOp::Unlink { parent, name }
        }
        3 => {
            let inode = d.u64();
            let chunk = decode_chunk_pointer(&mut d);
            MetaOp::AppendChunk { inode, chunk }
        }
        4 => MetaOp::BeginConvert { inode: d.u64() },
        5 => {
            let inode = d.u64();
            let n = d.u32();
            let extents = (0..n).map(|_| decode_extent(&mut d)).collect();
            MetaOp::CompleteConvert { inode, extents }
        }
        6 => {
            let inode = d.u64();
            let scheme = decode_redundancy_scheme(&mut d);
            MetaOp::SetRedundancyScheme { inode, scheme }
        }
        7 => MetaOp::PoolMapChange(decode_pool_map(&mut d)),
        8 => MetaOp::MetaGroupChange(decode_meta_group(&mut d)),
        9 => {
            let inode = d.u64();
            let n = d.u32();
            let extents = (0..n).map(|_| decode_extent(&mut d)).collect();
            MetaOp::UpdateExtents { inode, extents }
        }
        other => panic!("bad MetaOp tag {other}"),
    }
}

/// `InodeId` is just `u64` (`tartine_proto::InodeId`) — this alias exists
/// only so call sites in `store.rs` reading redb table values don't need
/// to spell out the underlying type.
pub type Inode = InodeId;

#[cfg(test)]
mod tests {
    use super::*;
    use tartine_proto::{DiskRoles, DiskState};

    fn sample_inode_record() -> InodeRecord {
        InodeRecord {
            inode: 42,
            mode: InodeMode::AppendOnly,
            size: 100,
            redundancy: RedundancyScheme::Replicated(vec![
                ReplicaSlot::AnyOfClass(Some(DiskClass::Ssd)),
                ReplicaSlot::Pinned(Uuid(7)),
            ]),
            data: DataLocator::ChunkLog(vec![ChunkPointer {
                chunk_seq: 0,
                replicas: vec![(Uuid(1), 0), (Uuid(2), 0)],
                len: 100,
                checksum: 0xdead_beef,
            }]),
            uid: 1000,
            gid: 1000,
            unix_mode: 0o644,
            mtime_unix: 1_700_000_000,
        }
    }

    #[test]
    fn inode_record_round_trips() {
        let rec = sample_inode_record();
        let mut e = Encoder::new();
        encode_inode_record(&mut e, &rec);
        let bytes = e.into_vec();
        let mut d = Decoder::new(&bytes);
        let decoded = decode_inode_record(&mut d);
        assert_eq!(decoded.inode, rec.inode);
        assert_eq!(decoded.mode, rec.mode);
        assert_eq!(decoded.redundancy, rec.redundancy);
    }

    #[test]
    fn every_meta_op_variant_round_trips() {
        let ops = vec![
            MetaOp::CreateInode(sample_inode_record()),
            MetaOp::Link {
                parent: 1,
                name: "foo".into(),
                child: 2,
            },
            MetaOp::Unlink {
                parent: 1,
                name: "foo".into(),
            },
            MetaOp::AppendChunk {
                inode: 2,
                chunk: ChunkPointer {
                    chunk_seq: 0,
                    replicas: vec![(Uuid(1), 0)],
                    len: 5,
                    checksum: 9,
                },
            },
            MetaOp::BeginConvert { inode: 2 },
            MetaOp::CompleteConvert {
                inode: 2,
                extents: vec![Extent {
                    file_offset: 0,
                    len: 5,
                    replicas: vec![(Uuid(1), 0)],
                    checksum: 9,
                }],
            },
            MetaOp::SetRedundancyScheme {
                inode: 2,
                scheme: RedundancyScheme::Replicated(vec![ReplicaSlot::AnyOfClass(None); 3]),
            },
            MetaOp::UpdateExtents {
                inode: 2,
                extents: vec![Extent {
                    file_offset: 0,
                    len: 5,
                    replicas: vec![(Uuid(1), 0)],
                    checksum: 9,
                }],
            },
        ];
        for op in ops {
            let bytes = encode_op(&op);
            let decoded = decode_op(&bytes);
            assert_eq!(format!("{op:?}"), format!("{decoded:?}"));
        }
    }

    #[test]
    fn pool_map_round_trips() {
        let mut disks = std::collections::HashMap::new();
        disks.insert(
            Uuid(1),
            DiskEntry {
                roles: DiskRoles {
                    data: true,
                    metadata: false,
                },
                state: DiskState::Active,
                class: DiskClass::Nvme,
                weight: 2.5,
                used_bytes: 10,
                capacity_bytes: 100,
            },
        );
        let pm = PoolMap { epoch: 3, disks };
        let mut e = Encoder::new();
        encode_pool_map(&mut e, &pm);
        let bytes = e.into_vec();
        let mut d = Decoder::new(&bytes);
        let decoded = decode_pool_map(&mut d);
        assert_eq!(decoded.epoch, pm.epoch);
        assert_eq!(decoded.disks[&Uuid(1)].weight, 2.5);
        assert_eq!(decoded.disks[&Uuid(1)].class, DiskClass::Nvme);
    }
}
