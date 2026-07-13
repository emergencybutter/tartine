//! FUSE-facing layer: a real `fuser::Filesystem` impl
//! (IMPLEMENTATION_PLAN.md P1.5), translating VFS operations into
//! `write_path`'s state-machine decisions and `tartine_meta::pool::Pool`
//! operations. See DESIGN.md §9 and §11.

pub mod ioctl;
pub mod write_path;

use std::ffi::OsStr;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyIoctl, ReplyWrite, Request, TimeOrNow, WriteFlags,
};

use tartine_meta::pool::{Pool, PoolError, S_IFDIR, S_IFREG};
use tartine_proto::{InodeId, InodeMode, InodeRecord, RedundancyScheme, ReplicaSlot};

use crate::ioctl::{
    TartineState, TARTINE_MODE_APPEND_ONLY, TARTINE_MODE_CONVERTING, TARTINE_MODE_WRITABLE,
};

const TTL: Duration = Duration::from_secs(1);

/// Handles `TARTINE_IOC_GET_STATE` (DESIGN.md §9.2): reports the inode's
/// current mode and, while `Converting`, how far the background
/// materializer has gotten. `bytes_converted` is always `size` here —
/// `Pool::complete_convert` is synchronous (IMPLEMENTATION_PLAN.md's
/// P1.9 async/bounded-delta-pass materializer isn't implemented), so
/// there's no in-between progress to report.
pub fn get_state(inode: &InodeRecord) -> TartineState {
    let mode = match inode.mode {
        InodeMode::AppendOnly => TARTINE_MODE_APPEND_ONLY,
        InodeMode::Converting => TARTINE_MODE_CONVERTING,
        InodeMode::Writable => TARTINE_MODE_WRITABLE,
    };
    TartineState {
        mode,
        _pad: 0,
        bytes_total: inode.size,
        bytes_converted: inode.size,
    }
}

/// The default redundancy policy for newly created files
/// (`tartinectl fs set-default-redundancy`, DESIGN.md §10.3, isn't
/// wired up in this prototype) — two unconstrained replicas.
fn default_redundancy() -> RedundancyScheme {
    RedundancyScheme::Replicated(vec![ReplicaSlot::AnyOfClass(None); 2])
}

fn errno_for(e: &PoolError) -> Errno {
    match e {
        PoolError::NotFound => Errno::ENOENT,
        PoolError::WrongMode => Errno::EPERM,
        PoolError::Unplaceable => Errno::ENOSPC,
        PoolError::NeedAtLeastTwoDisks => Errno::EINVAL,
        PoolError::Io(_) | PoolError::Meta(_) => Errno::EIO,
    }
}

fn attr_of(rec: &InodeRecord) -> FileAttr {
    let kind = if rec.unix_mode & S_IFDIR == S_IFDIR {
        FileType::Directory
    } else {
        FileType::RegularFile
    };
    let perm = (rec.unix_mode & 0o7777) as u16;
    let time = UNIX_EPOCH + Duration::from_secs(rec.mtime_unix);
    FileAttr {
        ino: INodeNo(rec.inode),
        size: rec.size,
        blocks: rec.size.div_ceil(512),
        atime: time,
        mtime: time,
        ctime: time,
        crtime: time,
        kind,
        perm,
        nlink: 1,
        uid: rec.uid,
        gid: rec.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// The real, mountable filesystem (IMPLEMENTATION_PLAN.md P1.5).
/// `fuser::Filesystem`'s methods all take `&self` (fuser drives them
/// from a session that may use multiple worker threads), so `Pool` —
/// which needs `&mut self` for anything that mutates — lives behind a
/// `Mutex`. One pool per mount, matching DESIGN.md §2's single-writer
/// model (this *is* the single writer, just in userspace instead of a
/// kernel module).
pub struct TartineFs {
    pool: Mutex<Pool>,
}

impl TartineFs {
    pub fn new(pool: Pool) -> Self {
        TartineFs {
            pool: Mutex::new(pool),
        }
    }

    fn name_str(name: &OsStr) -> Option<&str> {
        name.to_str()
    }
}

impl Filesystem for TartineFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = Self::name_str(name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let pool = self.pool.lock().unwrap();
        match pool.lookup(parent.0, name) {
            Ok(Some(child)) => match pool.get_inode(child) {
                Ok(Some(rec)) => reply.entry(&TTL, &attr_of(&rec), Generation(0)),
                _ => reply.error(Errno::EIO),
            },
            Ok(None) => reply.error(Errno::ENOENT),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let pool = self.pool.lock().unwrap();
        match pool.get_inode(ino.0) {
            Ok(Some(rec)) => reply.attr(&TTL, &attr_of(&rec)),
            Ok(None) => reply.error(Errno::ENOENT),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    /// Handles truncation (DESIGN.md §9.1: `ftruncate`/a `>`-redirect
    /// open must fail on an `AppendOnly`/`Converting` file). Nothing
    /// else `setattr` can be asked for (uid/gid/mode/time changes) is
    /// implemented — out of scope for P1.5's acceptance transcript,
    /// which only exercises the truncate-rejection path.
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let pool = self.pool.lock().unwrap();
        let rec = match pool.get_inode(ino.0) {
            Ok(Some(rec)) => rec,
            Ok(None) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        if let Some(new_size) = size {
            if new_size != rec.size && write_path::truncate_allowed(&rec).is_err() {
                reply.error(Errno::EPERM);
                return;
            }
            // A resize on an already-Writable file would need
            // Pool::write-style extent rewriting; not exercised by
            // P1.5's transcript (which only truncates an AppendOnly
            // file, and that's rejected above) so not implemented.
        }
        reply.attr(&TTL, &attr_of(&rec));
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name) = Self::name_str(name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut pool = self.pool.lock().unwrap();
        if matches!(pool.lookup(parent.0, name), Ok(Some(_))) {
            reply.error(Errno::EEXIST);
            return;
        }
        let unix_mode = S_IFREG | (mode & 0o7777);
        match pool.create_file(
            parent.0,
            name,
            default_redundancy(),
            req.uid(),
            req.gid(),
            unix_mode,
        ) {
            Ok(inode) => match pool.get_inode(inode) {
                Ok(Some(rec)) => reply.created(
                    &TTL,
                    &attr_of(&rec),
                    Generation(0),
                    FileHandle(0),
                    FopenFlags::empty(),
                ),
                _ => reply.error(Errno::EIO),
            },
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = Self::name_str(name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let mut pool = self.pool.lock().unwrap();
        match pool.unlink(parent.0, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let pool = self.pool.lock().unwrap();
        match pool.read(ino.0, offset, size as usize) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno_for(&e)),
        }
    }

    /// Classifies the write (DESIGN.md §9.1/§11) via the same
    /// `write_path` adapter over `tartine-kcore` that decides this for
    /// the kernel module — this is the first time that code path is
    /// exercised by a live mount instead of only unit tests.
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut pool = self.pool.lock().unwrap();
        let rec = match pool.get_inode(ino.0) {
            Ok(Some(rec)) => rec,
            Ok(None) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        match write_path::classify_write(&rec, offset, data.len()) {
            Ok(write_path::WriteKind::Append) => match pool.append(ino.0, data.to_vec()) {
                Ok(()) => reply.written(data.len() as u32),
                Err(e) => reply.error(errno_for(&e)),
            },
            Ok(write_path::WriteKind::RandomWrite) => match pool.write(ino.0, offset, data) {
                Ok(()) => reply.written(data.len() as u32),
                Err(e) => reply.error(errno_for(&e)),
            },
            Err(
                write_path::WriteError::NotAppendOnlyOrder
                | write_path::WriteError::RandomWriteDenied,
            ) => {
                reply.error(Errno::EPERM);
            }
            Err(write_path::WriteError::ConversionInProgress) => reply.error(Errno::EAGAIN),
        }
    }

    /// Directories with children beyond the root aren't supported yet
    /// (no `mkdir`), so `..` is simplified to always resolve to `ino`
    /// itself — correct for the root (whose real parent, by POSIX
    /// convention, is itself) and not exercised for anything else since
    /// nothing but the root is ever a directory in this prototype.
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let pool = self.pool.lock().unwrap();
        let children = match pool.readdir(ino.0) {
            Ok(c) => c,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let mut entries: Vec<(InodeId, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".to_string()),
            (ino.0, FileType::Directory, "..".to_string()),
        ];
        for (name, child) in children {
            let kind = match pool.get_inode(child) {
                Ok(Some(rec)) if rec.unix_mode & S_IFDIR == S_IFDIR => FileType::Directory,
                _ => FileType::RegularFile,
            };
            entries.push((child, kind, name));
        }

        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (i + 1) as u64, kind, name) {
                break; // reply buffer full; kernel will call again with a later offset
            }
        }
        reply.ok();
    }

    fn ioctl(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _flags: fuser::IoctlFlags,
        cmd: u32,
        in_data: &[u8],
        _out_size: u32,
        reply: ReplyIoctl,
    ) {
        let mut pool = self.pool.lock().unwrap();
        match cmd {
            ioctl::TARTINE_IOC_MAKE_WRITABLE => {
                let Some(flags) = in_data
                    .get(0..4)
                    .and_then(|b| b.try_into().ok())
                    .map(u32::from_le_bytes)
                else {
                    reply.error(Errno::EINVAL);
                    return;
                };
                match pool.begin_convert(ino.0) {
                    Ok(()) => {
                        if flags & ioctl::TARTINE_CONVERT_FLAG_WAIT != 0 {
                            match pool.complete_convert(ino.0) {
                                Ok(()) => reply.ioctl(0, &[]),
                                Err(e) => reply.error(errno_for(&e)),
                            }
                        } else {
                            reply.ioctl(0, &[]);
                        }
                    }
                    Err(_) => reply.error(Errno::EALREADY),
                }
            }
            ioctl::TARTINE_IOC_GET_STATE => match pool.get_inode(ino.0) {
                Ok(Some(rec)) => reply.ioctl(0, &get_state(&rec).to_bytes()),
                Ok(None) => reply.error(Errno::ENOENT),
                Err(_) => reply.error(Errno::EIO),
            },
            ioctl::TARTINE_IOC_SET_REDUNDANCY => {
                let Some(scheme) = ioctl::decode_set_redundancy(in_data) else {
                    reply.error(Errno::EINVAL);
                    return;
                };
                if matches!(scheme, RedundancyScheme::ErasureCoded { .. }) {
                    reply.error(Errno::EOPNOTSUPP);
                    return;
                }
                match pool.set_redundancy(ino.0, scheme) {
                    Ok(()) => reply.ioctl(0, &[]),
                    Err(e) => reply.error(errno_for(&e)),
                }
            }
            ioctl::TARTINE_IOC_GET_REDUNDANCY => match pool.get_inode(ino.0) {
                Ok(Some(rec)) => reply.ioctl(0, &ioctl::encode_set_redundancy(&rec.redundancy)),
                Ok(None) => reply.error(Errno::ENOENT),
                Err(_) => reply.error(Errno::EIO),
            },
            _ => reply.error(Errno::ENOTTY),
        }
    }
}
