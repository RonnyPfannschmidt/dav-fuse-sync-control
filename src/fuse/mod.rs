//! The FUSE layer: shims over [`crate::vfs::Vfs`].
//!
//! Every handler copies its borrowed arguments, then spawns a task that does
//! the work and replies. The session loop returns within microseconds, so one
//! slow request no longer stalls the whole filesystem.
//!
//! Three rules make this correct, and each is a review-checklist item:
//!
//! 1. **Copy every borrowed argument before spawning.** `&OsStr` and `&[u8]`
//!    point into the session's reused receive buffer, which the very next
//!    request overwrites. This is the one way to get memory corruption here.
//! 2. **`forget` stays inline.** It takes no reply and needs no I/O, and
//!    running it on the loop thread preserves its ordering against the lookups
//!    it refers to.
//! 3. **Mutating operations reply `EROFS`, never `ENOSYS`.** `ENOSYS` makes the
//!    kernel permanently disable that operation for the lifetime of the mount,
//!    which would silently poison a later read-write rollout.

pub mod attr;

pub use attr::{to_file_attr, NodeMeta, OwnerIds};

use std::ffi::OsStr;
use std::sync::Arc;

use fuser::{
    Errno, FileHandle, Generation, INodeNo, LockOwner, OpenFlags, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyXattr,
    Request,
};

use crate::vfs::Vfs;

pub struct DavFS {
    vfs: Arc<Vfs>,
    /// Captured explicitly rather than via `Handle::current()`, which is only
    /// available when the caller happens to be inside a runtime.
    rt: tokio::runtime::Handle,
}

impl DavFS {
    pub fn new(rt: tokio::runtime::Handle, vfs: Arc<Vfs>) -> Self {
        Self { vfs, rt }
    }
}

/// Spawn the body, replying `EAGAIN` if too many requests are already in
/// flight. Bounded rather than unbounded queueing: a pathological client should
/// be slowed down, not allowed to exhaust memory.
macro_rules! dispatch {
    ($self:ident, $reply:ident, |$vfs:ident| $body:expr) => {{
        let Some(_permit) = $self.vfs.try_admit() else {
            $reply.error(Errno::EAGAIN);
            return;
        };
        let $vfs = $self.vfs.clone();
        $self.rt.spawn(async move {
            let _permit = _permit;
            $body
        });
    }};
}

impl fuser::Filesystem for DavFS {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        // MUST copy: `name` borrows the session's receive buffer.
        let Some(name) = name.to_str().map(str::to_owned) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let ttl = self.vfs.attr_ttl();

        dispatch!(self, reply, |vfs| {
            match vfs.lookup(parent.0, &name).await {
                Ok(attr) => reply.entry(&ttl, &attr, Generation(0)),
                Err(e) => reply.error(e.to_errno()),
            }
        });
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ttl = self.vfs.attr_ttl();
        dispatch!(self, reply, |vfs| {
            match vfs.getattr(ino.0).await {
                Ok(attr) => reply.attr(&ttl, &attr),
                Err(e) => reply.error(e.to_errno()),
            }
        });
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        dispatch!(self, reply, |vfs| {
            match vfs.readdir(ino.0).await {
                Ok(entries) => {
                    for (i, (child_ino, kind, name)) in
                        entries.into_iter().enumerate().skip(offset as usize)
                    {
                        if reply.add(INodeNo(child_ino), (i + 1) as u64, kind, &name) {
                            break;
                        }
                    }
                    reply.ok();
                }
                Err(e) => reply.error(e.to_errno()),
            }
        });
    }

    /// Attributes are filled in during the listing, so `ls` is not followed by
    /// a lookup storm.
    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let ttl = self.vfs.attr_ttl();
        dispatch!(self, reply, |vfs| {
            match vfs.readdirplus(ino.0).await {
                Ok(entries) => {
                    for (i, (child_ino, _kind, name, attr)) in
                        entries.into_iter().enumerate().skip(offset as usize)
                    {
                        if reply.add(
                            INodeNo(child_ino),
                            (i + 1) as u64,
                            &name,
                            &ttl,
                            &attr,
                            Generation(0),
                        ) {
                            break;
                        }
                    }
                    reply.ok();
                }
                Err(e) => reply.error(e.to_errno()),
            }
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let write = flags.0 & (libc::O_WRONLY | libc::O_RDWR) != 0;
        dispatch!(self, reply, |vfs| {
            match vfs.open(ino.0, write).await {
                Ok(fh) => reply.opened(FileHandle(fh), fuser::FopenFlags::empty()),
                Err(e) => reply.error(e.to_errno()),
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        dispatch!(self, reply, |vfs| {
            match vfs.read(fh.0, offset, size).await {
                Ok(data) => reply.data(&data),
                Err(e) => reply.error(e.to_errno()),
            }
        });
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.vfs.release(fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        dispatch!(self, reply, |vfs| {
            let s = vfs.statfs().await;
            reply.statfs(
                s.blocks, s.bfree, s.bfree, s.files, s.ffree, s.bsize, s.namelen, s.bsize,
            );
        });
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        let buf = self.vfs.listxattr();
        reply_bytes(reply, &buf, size);
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(name) = name.to_str().map(str::to_owned) else {
            reply.error(Errno::ENODATA);
            return;
        };
        dispatch!(self, reply, |vfs| {
            match vfs.getxattr(ino.0, &name).await {
                Ok(value) => reply_bytes(reply, &value, size),
                // An unknown attribute is ENODATA, not ENOENT: the file exists.
                Err(crate::RemoteError::NotFound) => reply.error(Errno::ENODATA),
                Err(e) => reply.error(e.to_errno()),
            }
        });
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let Some(name) = name.to_str().map(str::to_owned) else {
            reply.error(Errno::ENOTSUP);
            return;
        };
        // MUST copy: `value` borrows the session's receive buffer.
        let value = value.to_vec();

        dispatch!(self, reply, |vfs| {
            match vfs.setxattr(ino.0, &name, &value).await {
                Ok(()) => reply.ok(),
                Err(crate::RemoteError::Unsupported(_)) => reply.error(Errno::ENOTSUP),
                Err(e) => reply.error(e.to_errno()),
            }
        });
    }

    // -- mutating operations: EROFS, deliberately not ENOSYS ---------------

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: fuser::ReplyWrite,
    ) {
        reply.error(Errno::EROFS);
    }

    fn create(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mkdir(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }
}

/// The getxattr/listxattr size protocol: a zero size asks for the length.
fn reply_bytes(reply: ReplyXattr, data: &[u8], size: u32) {
    if size == 0 {
        reply.size(data.len() as u32);
    } else if size as usize >= data.len() {
        reply.data(data);
    } else {
        reply.error(Errno::ERANGE);
    }
}
