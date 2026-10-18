//! The FUSE adapter: forwards kernel calls to a [`FsOps`].
//!
//! Linux uses `fuser` with its built-in mount code (needs `/dev/fuse` and the
//! setuid `fusermount3` helper). macOS uses the same adapter against FUSE-T
//! (preferred) or macFUSE; see [`platform_options`]. The `op_*` methods hold
//! all the argument handling and return `Result<_, Errno>`, so they are tested
//! without a kernel; the `Filesystem` impl below them only replies.

use super::translate::{time_to_nanos, to_errno, to_fuse_attr};
use super::{Mount, Mounter};
use crate::error::Error;
use fuser::{
    AccessFlags, BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLock, ReplyOpen,
    ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use startfs_core::clock::{Clock, SystemClock};
use startfs_core::ops::{Attr, FsError, FsOps, Ino, SetAttr};
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// How long the kernel may cache attributes and names. Short, because other
/// nodes' changes arrive through the daemon, not through the kernel.
const TTL: Duration = Duration::from_secs(1);
const BLOCK: u32 = 4096;

/// One entry of a readdir reply: inode, offset of the next entry, kind, name.
pub type DirRow = (Ino, u64, FileType, String);

#[derive(Debug, PartialEq, Eq)]
pub enum XattrReply {
    Size(u32),
    Data(Vec<u8>),
}

pub struct FuseAdapter {
    fs: Arc<dyn FsOps>,
    clock: Arc<dyn Clock>,
}

fn name(n: &OsStr) -> Result<&str, Errno> {
    n.to_str().ok_or(Errno::EINVAL)
}

fn ok<T>(r: Result<T, FsError>) -> Result<T, Errno> {
    r.map_err(|e| to_errno(&e))
}

impl FuseAdapter {
    pub fn new(fs: Arc<dyn FsOps>) -> Self {
        Self::with_clock(fs, Arc::new(SystemClock))
    }

    pub fn with_clock(fs: Arc<dyn FsOps>, clock: Arc<dyn Clock>) -> Self {
        Self { fs, clock }
    }

    pub fn op_lookup(&self, parent: u64, n: &OsStr) -> Result<Attr, Errno> {
        ok(self.fs.lookup(parent, name(n)?))
    }

    pub fn op_getattr(&self, ino: u64) -> Result<Attr, Errno> {
        ok(self.fs.getattr(ino))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn op_setattr(
        &self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        mtime: Option<TimeOrNow>,
    ) -> Result<Attr, Errno> {
        let now = self.clock.now();
        let set = SetAttr {
            mode: mode.map(|m| m & 0o7777),
            size,
            mtime: mtime.map(|t| time_to_nanos(t, now)),
            uid,
            gid,
        };
        ok(self.fs.setattr(ino, set))
    }

    pub fn op_mkdir(&self, parent: u64, n: &OsStr, mode: u32, umask: u32) -> Result<Attr, Errno> {
        ok(self.fs.mkdir(parent, name(n)?, mode & !umask & 0o7777))
    }

    pub fn op_create(&self, parent: u64, n: &OsStr, mode: u32, umask: u32) -> Result<Attr, Errno> {
        ok(self.fs.create(parent, name(n)?, mode & !umask & 0o7777))
    }

    pub fn op_symlink(&self, parent: u64, n: &OsStr, target: &Path) -> Result<Attr, Errno> {
        let target = target.to_str().ok_or(Errno::EINVAL)?;
        ok(self.fs.symlink(parent, name(n)?, target))
    }

    pub fn op_link(&self, ino: u64, new_parent: u64, n: &OsStr) -> Result<Attr, Errno> {
        ok(self.fs.link(ino, new_parent, name(n)?))
    }

    pub fn op_readlink(&self, ino: u64) -> Result<Vec<u8>, Errno> {
        ok(self.fs.readlink(ino)).map(String::into_bytes)
    }

    pub fn op_read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        ok(self.fs.read(ino, offset, size))
    }

    pub fn op_write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32, Errno> {
        ok(self.fs.write(ino, offset, data)).map(|n| n as u32)
    }

    /// Commit the file's buffered writes (flush and release).
    pub fn op_flush(&self, ino: u64) -> Result<(), Errno> {
        ok(self.fs.flush(ino))
    }

    /// Commit the file's buffered writes and make everything committed durable.
    pub fn op_fsync(&self, ino: u64) -> Result<(), Errno> {
        ok(self.fs.fsync(ino))
    }

    pub fn op_unlink(&self, parent: u64, n: &OsStr) -> Result<(), Errno> {
        ok(self.fs.unlink(parent, name(n)?))
    }

    pub fn op_rmdir(&self, parent: u64, n: &OsStr) -> Result<(), Errno> {
        ok(self.fs.rmdir(parent, name(n)?))
    }

    /// `renameat2` flags (no-replace, exchange) are not supported.
    pub fn op_rename(
        &self,
        parent: u64,
        n: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        flags: RenameFlags,
    ) -> Result<(), Errno> {
        if !flags.is_empty() {
            return Err(Errno::EINVAL);
        }
        ok(self
            .fs
            .rename(parent, name(n)?, new_parent, name(new_name)?))
    }

    /// Entries after `offset`, led by `.` and `..` (the core lists neither).
    /// An entry's offset is the position of the entry after it.
    pub fn op_readdir(&self, ino: u64, offset: u64) -> Result<Vec<DirRow>, Errno> {
        let mut rows: Vec<(Ino, FileType, String)> = vec![
            (ino, FileType::Directory, ".".into()),
            (ino, FileType::Directory, "..".into()),
        ];
        for e in ok(self.fs.readdir(ino))? {
            rows.push((e.ino, super::translate::kind_to_fuse(e.kind), e.name));
        }
        Ok(rows
            .into_iter()
            .enumerate()
            .skip(offset as usize)
            .map(|(i, (ino, kind, name))| (ino, i as u64 + 1, kind, name))
            .collect())
    }

    pub fn op_setxattr(&self, ino: u64, n: &OsStr, value: &[u8]) -> Result<(), Errno> {
        ok(self.fs.setxattr(ino, name(n)?, value))
    }

    /// `size == 0` asks for the length only; a too-small buffer is `ERANGE`.
    pub fn op_getxattr(&self, ino: u64, n: &OsStr, size: u32) -> Result<XattrReply, Errno> {
        xattr_reply(ok(self.fs.getxattr(ino, name(n)?))?, size)
    }

    pub fn op_listxattr(&self, ino: u64, size: u32) -> Result<XattrReply, Errno> {
        let mut packed = Vec::new();
        for n in ok(self.fs.listxattr(ino))? {
            packed.extend_from_slice(n.as_bytes());
            packed.push(0);
        }
        xattr_reply(packed, size)
    }

    pub fn op_removexattr(&self, ino: u64, n: &OsStr) -> Result<(), Errno> {
        ok(self.fs.removexattr(ino, name(n)?))
    }

    /// `(blocks, free blocks)` of [`BLOCK`] bytes.
    pub fn op_statfs(&self) -> Result<(u64, u64), Errno> {
        let space = ok(self.fs.space())?;
        let block = u64::from(BLOCK);
        let blocks = space.total_bytes.div_ceil(block);
        let free = blocks - space.used_bytes.div_ceil(block).min(blocks);
        Ok((blocks, free))
    }
}

fn xattr_reply(data: Vec<u8>, size: u32) -> Result<XattrReply, Errno> {
    if size == 0 {
        Ok(XattrReply::Size(data.len() as u32))
    } else if data.len() > size as usize {
        Err(Errno::ERANGE)
    } else {
        Ok(XattrReply::Data(data))
    }
}

fn entry(r: Result<Attr, Errno>, reply: ReplyEntry) {
    match r {
        Ok(a) => reply.entry(&TTL, &to_fuse_attr(&a), Generation(0)),
        Err(e) => reply.error(e),
    }
}

fn empty(r: Result<(), Errno>, reply: ReplyEmpty) {
    match r {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e),
    }
}

fn xattr(r: Result<XattrReply, Errno>, reply: ReplyXattr) {
    match r {
        Ok(XattrReply::Size(n)) => reply.size(n),
        Ok(XattrReply::Data(d)) => reply.data(&d),
        Err(e) => reply.error(e),
    }
}

impl Filesystem for FuseAdapter {
    fn lookup(&self, _req: &Request, parent: INodeNo, n: &OsStr, reply: ReplyEntry) {
        entry(self.op_lookup(parent.0, n), reply);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.op_getattr(ino.0) {
            Ok(a) => reply.attr(&TTL, &to_fuse_attr(&a)),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        match self.op_setattr(ino.0, mode, uid, gid, size, mtime) {
            Ok(a) => reply.attr(&TTL, &to_fuse_attr(&a)),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.op_readlink(ino.0) {
            Ok(t) => reply.data(&t),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        n: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        entry(self.op_mkdir(parent.0, n, mode, umask), reply);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, n: &OsStr, reply: ReplyEmpty) {
        empty(self.op_unlink(parent.0, n), reply);
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, n: &OsStr, reply: ReplyEmpty) {
        empty(self.op_rmdir(parent.0, n), reply);
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        entry(self.op_symlink(parent.0, link_name, target), reply);
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        n: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        empty(
            self.op_rename(parent.0, n, newparent.0, newname, flags),
            reply,
        );
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        entry(self.op_link(ino.0, newparent.0, newname), reply);
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
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
        match self.op_read(ino.0, offset, size) {
            Ok(d) => reply.data(&d),
            Err(e) => reply.error(e),
        }
    }

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
        match self.op_write(ino.0, offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.op_flush(ino.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // The kernel flushes on close and reports that error; this covers handles
        // released without one, which have nobody to tell.
        if let Err(e) = self.op_flush(ino.0) {
            eprintln!(
                "startfs: release of inode {} could not commit writes: {e:?}",
                ino.0
            );
        }
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.op_fsync(ino.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        match self.op_readdir(ino.0, offset) {
            Ok(rows) => {
                for (child, next, kind, n) in rows {
                    if reply.add(INodeNo(child), next, kind, n) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.op_statfs() {
            Ok((blocks, free)) => reply.statfs(blocks, free, free, 0, 0, BLOCK, 255, BLOCK),
            Err(e) => reply.error(e),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        n: &OsStr,
        value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        empty(self.op_setxattr(ino.0, n, value), reply);
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, n: &OsStr, size: u32, reply: ReplyXattr) {
        xattr(self.op_getxattr(ino.0, n, size), reply);
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        xattr(self.op_listxattr(ino.0, size), reply);
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, n: &OsStr, reply: ReplyEmpty) {
        empty(self.op_removexattr(ino.0, n), reply);
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        // Permission bits are enforced by the kernel (`default_permissions`).
        reply.ok();
    }

    // Advisory locks (fcntl, flock) are local to each machine and are not
    // coordinated between nodes. Answering ENOSYS tells the kernel to handle
    // them itself, locally, without a warning per call.
    fn getlk(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        _start: u64,
        _end: u64,
        _typ: i32,
        _pid: u32,
        reply: ReplyLock,
    ) {
        reply.error(Errno::ENOSYS);
    }

    fn setlk(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        _start: u64,
        _end: u64,
        _typ: i32,
        _pid: u32,
        _sleep: bool,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOSYS);
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        n: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        match self.op_create(parent.0, n, mode, umask) {
            Ok(a) => {
                let attr: FileAttr = to_fuse_attr(&a);
                reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(0),
                    FopenFlags::empty(),
                );
            }
            Err(e) => reply.error(e),
        }
    }
}

/// Mount options for this platform.
///
/// macOS: FUSE-T and macFUSE both speak the libfuse API that `fuser` links on
/// macOS, so the same adapter serves both. `noappledouble` stops Finder
/// writing `._*` sidecar files, `volname` names the volume. UNTESTED on this
/// project's Linux development machine: compiled only under `target_os =
/// "macos"`.
pub fn platform_options() -> Vec<MountOption> {
    let mut o = vec![
        MountOption::FSName("startfs".into()),
        MountOption::DefaultPermissions,
    ];
    #[cfg(target_os = "macos")]
    {
        o.push(MountOption::CUSTOM("volname=StartFS".into()));
        o.push(MountOption::CUSTOM("noappledouble".into()));
    }
    #[cfg(not(target_os = "macos"))]
    o.push(MountOption::Subtype("startfs".into()));
    o
}

/// Mounts through FUSE: `fuser` on Linux, FUSE-T or macFUSE on macOS.
pub struct FuseMounter;

/// How long a stop waits for the kernel to let the mount go before it detaches
/// it lazily and gives up on the session thread.
const UNMOUNT_GRACE: Duration = Duration::from_secs(3);

struct FuseMount {
    session: fuser::BackgroundSession,
    mountpoint: std::path::PathBuf,
}

impl Mount for FuseMount {
    /// A process with its working directory or an open file on the mount keeps
    /// the session thread from finishing, so waiting for it can take forever.
    /// After a grace period the mount is detached lazily and the caller carries
    /// on; exiting closes `/dev/fuse` and ends the session.
    fn unmount(self: Box<Self>) -> Result<(), Error> {
        let FuseMount {
            session,
            mountpoint,
        } = *self;
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "the receiver is gone once the grace period has passed"
            )]
            let _ = done.send(session.umount_and_join());
        });
        match finished.recv_timeout(UNMOUNT_GRACE) {
            Ok(result) => result.map_err(Error::Unmount),
            Err(_) => {
                eprintln!(
                    "startfs: {} is in use; detaching it instead of waiting",
                    mountpoint.display()
                );
                lazy_unmount(&mountpoint);
                Ok(())
            }
        }
    }
}

/// `fusermount -uz`: detach the mount now, whatever still uses it.
fn lazy_unmount(mountpoint: &Path) {
    for tool in ["fusermount3", "fusermount"] {
        if std::process::Command::new(tool)
            .arg("-uz")
            .arg(mountpoint)
            .status()
            .is_ok_and(|s| s.success())
        {
            return;
        }
    }
    eprintln!("startfs: could not detach {}", mountpoint.display());
}

impl Mounter for FuseMounter {
    fn mount(&self, fs: Arc<dyn FsOps>, mountpoint: &Path) -> Result<Box<dyn Mount>, Error> {
        let mut config = Config::default();
        config.mount_options = platform_options();
        let session =
            fuser::spawn_mount(FuseAdapter::new(fs), mountpoint, &config).map_err(|source| {
                Error::Mount {
                    mountpoint: mountpoint.to_path_buf(),
                    source,
                }
            })?;
        Ok(Box::new(FuseMount {
            session,
            mountpoint: mountpoint.to_path_buf(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use startfs_core::fs::{LocalFs, NodeConfig};
    use startfs_core::members::MemberId;
    use startfs_core::ops::ROOT_INO;

    fn adapter() -> (FuseAdapter, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeConfig {
            node: MemberId(0),
            uid: 1000,
            gid: 1000,
        };
        let fs = LocalFs::open(dir.path(), node, 1 << 20, Arc::new(SystemClock)).unwrap();
        (FuseAdapter::new(Arc::new(fs)), dir)
    }

    fn os(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    #[test]
    fn a_file_can_be_created_written_and_read_back() {
        let (a, _d) = adapter();
        let f = a.op_create(ROOT_INO, os("a.txt"), 0o666, 0o022).unwrap();
        assert_eq!(f.mode, 0o644, "umask applies to the requested mode");
        assert_eq!(a.op_write(f.ino, 0, b"hello").unwrap(), 5);
        assert_eq!(a.op_read(f.ino, 1, 100).unwrap(), b"ello");
        assert_eq!(a.op_lookup(ROOT_INO, os("a.txt")).unwrap().size, 5);
    }

    #[test]
    fn flush_commits_buffered_writes_and_is_a_no_op_with_nothing_buffered() {
        let (a, _d) = adapter();
        let f = a.op_create(ROOT_INO, os("b.txt"), 0o644, 0).unwrap();
        a.op_write(f.ino, 0, b"data").unwrap();
        assert_eq!(a.op_flush(f.ino), Ok(()));
        assert_eq!(a.op_read(f.ino, 0, 10).unwrap(), b"data");
        assert_eq!(
            a.op_flush(999_999),
            Ok(()),
            "nothing buffered, nothing to do"
        );
    }

    #[test]
    fn failures_carry_the_right_errno() {
        let (a, _d) = adapter();
        assert_eq!(
            a.op_lookup(ROOT_INO, os("nope")).unwrap_err(),
            Errno::ENOENT
        );
        use std::os::unix::ffi::OsStrExt;
        let bad = OsStr::from_bytes(&[0xff, 0xfe]);
        assert_eq!(a.op_lookup(ROOT_INO, bad).unwrap_err(), Errno::EINVAL);
        a.op_mkdir(ROOT_INO, os("d"), 0o755, 0).unwrap();
        a.op_create(ROOT_INO, os("f"), 0o644, 0).unwrap();
        assert_eq!(a.op_rmdir(ROOT_INO, os("f")).unwrap_err(), Errno::ENOTDIR);
        let f = a.op_lookup(ROOT_INO, os("f")).unwrap();
        let d = a.op_lookup(ROOT_INO, os("d")).unwrap();
        assert_eq!(
            a.op_link(d.ino, ROOT_INO, os("d2")).unwrap_err(),
            Errno::EPERM
        );
        assert_eq!(
            a.op_link(f.ino, ROOT_INO, os("d")).unwrap_err(),
            Errno::EEXIST
        );
    }

    #[test]
    fn rename_with_special_flags_is_refused_and_plain_rename_works() {
        let (a, _d) = adapter();
        a.op_create(ROOT_INO, os("x"), 0o644, 0).unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(
            a.op_rename(
                ROOT_INO,
                os("x"),
                ROOT_INO,
                os("y"),
                RenameFlags::RENAME_EXCHANGE
            )
            .unwrap_err(),
            Errno::EINVAL
        );
        a.op_rename(ROOT_INO, os("x"), ROOT_INO, os("y"), RenameFlags::empty())
            .unwrap();
        assert!(a.op_lookup(ROOT_INO, os("y")).is_ok());
        assert_eq!(a.op_lookup(ROOT_INO, os("x")).unwrap_err(), Errno::ENOENT);
    }

    #[test]
    fn readdir_lists_dots_first_and_resumes_from_an_offset() {
        let (a, _d) = adapter();
        a.op_create(ROOT_INO, os("one"), 0o644, 0).unwrap();
        a.op_mkdir(ROOT_INO, os("two"), 0o755, 0).unwrap();
        let all = a.op_readdir(ROOT_INO, 0).unwrap();
        let names: Vec<_> = all.iter().map(|r| r.3.as_str()).collect();
        assert_eq!(names, [".", "..", "one", "two"]);
        assert_eq!(all[3].2, FileType::Directory);
        let rest = a.op_readdir(ROOT_INO, all[1].1).unwrap();
        let names: Vec<_> = rest.iter().map(|r| r.3.as_str()).collect();
        assert_eq!(names, ["one", "two"]);
    }

    #[test]
    fn setattr_truncates_and_sets_mtime() {
        let (a, _d) = adapter();
        let f = a.op_create(ROOT_INO, os("t"), 0o644, 0).unwrap();
        a.op_write(f.ino, 0, b"0123456789").unwrap();
        let when = TimeOrNow::SpecificTime(std::time::UNIX_EPOCH + Duration::from_secs(5));
        let at = a
            .op_setattr(f.ino, Some(0o100600), None, None, Some(4), Some(when))
            .unwrap();
        assert_eq!((at.size, at.mode, at.mtime), (4, 0o600, 5_000_000_000));
    }

    #[test]
    fn symlinks_and_xattrs_round_trip() {
        let (a, _d) = adapter();
        let l = a
            .op_symlink(ROOT_INO, os("ln"), Path::new("target/x"))
            .unwrap();
        assert_eq!(a.op_readlink(l.ino).unwrap(), b"target/x");
        let f = a.op_create(ROOT_INO, os("f"), 0o644, 0).unwrap();
        a.op_setxattr(f.ino, os("user.k"), b"value").unwrap();
        assert_eq!(
            a.op_getxattr(f.ino, os("user.k"), 0).unwrap(),
            XattrReply::Size(5)
        );
        assert_eq!(
            a.op_getxattr(f.ino, os("user.k"), 2).unwrap_err(),
            Errno::ERANGE
        );
        assert_eq!(
            a.op_getxattr(f.ino, os("user.k"), 16).unwrap(),
            XattrReply::Data(b"value".to_vec())
        );
        assert_eq!(
            a.op_listxattr(f.ino, 64).unwrap(),
            XattrReply::Data(b"user.k\0".to_vec())
        );
        a.op_removexattr(f.ino, os("user.k")).unwrap();
        assert_eq!(
            a.op_getxattr(f.ino, os("user.k"), 0).unwrap_err().code(),
            libc::ENODATA
        );
    }

    #[test]
    fn statfs_reports_logical_used_blocks_within_the_usable_size() {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeConfig {
            node: MemberId(0),
            uid: 1000,
            gid: 1000,
        };
        let fs = Arc::new(LocalFs::open(dir.path(), node, 1 << 20, Arc::new(SystemClock)).unwrap());
        let a = FuseAdapter::new(fs.clone());
        let f = fs.create(ROOT_INO, "f", 0o644).unwrap();
        fs.write(f.ino, 0, &[1u8; 10_000]).unwrap();
        assert_eq!(a.op_statfs().unwrap(), (3, 0), "no cluster: used only");
        fs.set_usable_bytes(1 << 20);
        assert_eq!(a.op_statfs().unwrap(), (256, 253));
    }
}
