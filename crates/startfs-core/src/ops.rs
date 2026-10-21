//! The platform-independent filesystem-operations interface.
//!
//! Mount adapters (FUSE, FUSE-T) translate kernel calls into this trait; tests
//! drive the system only through it.

use std::sync::Arc;

pub type Ino = u64;
pub const ROOT_INO: Ino = 1;

/// Nanoseconds since the Unix epoch.
pub type Nanos = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub ino: Ino,
    pub kind: FileKind,
    /// Permission bits only (`0o7777` mask).
    pub mode: u32,
    pub nlink: u32,
    /// Ownership is always mapped to the local user of the node.
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub mtime: Nanos,
    pub ctime: Nanos,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub ino: Ino,
    pub kind: FileKind,
}

#[derive(Debug, Clone, Default)]
pub struct SetAttr {
    pub mode: Option<u32>,
    pub size: Option<u64>,
    pub mtime: Option<Nanos>,
    /// Accepted for compatibility, but ownership stays mapped to the local user.
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// Space actually consumed by the node's chunk store (like `df` for the store).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StoreStats {
    pub chunks: u64,
    pub stored_bytes: u64,
}

/// The filesystem's size as a whole, as `df` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Space {
    /// Bytes the files in the tree add up to.
    pub used_bytes: u64,
    /// What the cluster can hold, never less than `used_bytes`.
    pub total_bytes: u64,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum FsError {
    #[error("no such file or directory")]
    NotFound,
    #[error("file exists")]
    Exists,
    #[error("not a directory")]
    NotDir,
    #[error("is a directory")]
    IsDir,
    #[error("directory not empty")]
    NotEmpty,
    #[error("invalid argument")]
    Invalid,
    #[error("name too long")]
    NameTooLong,
    #[error("operation not supported")]
    NotSupported,
    #[error("operation not permitted")]
    NotPermitted,
    #[error("no such attribute")]
    NoData,
    #[error("file contents are not available: no reachable peer holds them")]
    Unavailable,
    #[error("file contents are not available: a peer that may hold them did not answer in time")]
    TimedOut,
    #[error("no space left on this node")]
    NoSpace,
    #[error("node is not running")]
    NodeDown,
    #[error("i/o error: {0}")]
    Io(String),
}

impl FsError {
    /// The POSIX errno a mount adapter should return.
    pub fn errno(&self) -> i32 {
        match self {
            FsError::NotFound => libc::ENOENT,
            FsError::Exists => libc::EEXIST,
            FsError::NotDir => libc::ENOTDIR,
            FsError::IsDir => libc::EISDIR,
            FsError::NotEmpty => libc::ENOTEMPTY,
            FsError::Invalid => libc::EINVAL,
            FsError::NameTooLong => libc::ENAMETOOLONG,
            FsError::NotSupported => libc::ENOTSUP,
            FsError::NotPermitted => libc::EPERM,
            FsError::NoData => libc::ENODATA,
            FsError::Unavailable | FsError::TimedOut => libc::EHOSTUNREACH,
            FsError::NoSpace => libc::ENOSPC,
            FsError::NodeDown => libc::ENOTCONN,
            FsError::Io(_) => libc::EIO,
        }
    }
}

pub type FsResult<T> = Result<T, FsError>;

pub trait FsOps: Send + Sync {
    fn lookup(&self, parent: Ino, name: &str) -> FsResult<Attr>;
    fn getattr(&self, ino: Ino) -> FsResult<Attr>;
    fn setattr(&self, ino: Ino, set: SetAttr) -> FsResult<Attr>;
    fn mkdir(&self, parent: Ino, name: &str, mode: u32) -> FsResult<Attr>;
    fn create(&self, parent: Ino, name: &str, mode: u32) -> FsResult<Attr>;
    fn symlink(&self, parent: Ino, name: &str, target: &str) -> FsResult<Attr>;
    fn readlink(&self, ino: Ino) -> FsResult<String>;
    /// Hardlinks are unsupported and always fail with a clear error.
    fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> FsResult<Attr>;
    fn read(&self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>>;
    /// Writes are buffered and committed in batches; the data is visible to
    /// reads at once but is only durable after `flush` (or a later commit).
    fn write(&self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<usize>;
    /// Commit this file's buffered writes (flush and release map here; they are durable only after `fsync` or a background sync).
    fn flush(&self, ino: Ino) -> FsResult<()>;
    /// Like `flush`, and everything committed so far is on disk when it returns.
    fn fsync(&self, ino: Ino) -> FsResult<()>;
    fn unlink(&self, parent: Ino, name: &str) -> FsResult<()>;
    fn rmdir(&self, parent: Ino, name: &str) -> FsResult<()>;
    fn rename(&self, parent: Ino, name: &str, new_parent: Ino, new_name: &str) -> FsResult<()>;
    fn readdir(&self, ino: Ino) -> FsResult<Vec<DirEntry>>;
    fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> FsResult<()>;
    fn getxattr(&self, ino: Ino, name: &str) -> FsResult<Vec<u8>>;
    fn listxattr(&self, ino: Ino) -> FsResult<Vec<String>>;
    fn removexattr(&self, ino: Ino, name: &str) -> FsResult<()>;
    fn stats(&self) -> FsResult<StoreStats>;
    /// The whole filesystem's size: logical bytes used and cluster capacity.
    fn space(&self) -> FsResult<Space>;
}

impl<T: FsOps + ?Sized> FsOps for Arc<T> {
    fn lookup(&self, p: Ino, n: &str) -> FsResult<Attr> {
        (**self).lookup(p, n)
    }
    fn getattr(&self, i: Ino) -> FsResult<Attr> {
        (**self).getattr(i)
    }
    fn setattr(&self, i: Ino, s: SetAttr) -> FsResult<Attr> {
        (**self).setattr(i, s)
    }
    fn mkdir(&self, p: Ino, n: &str, m: u32) -> FsResult<Attr> {
        (**self).mkdir(p, n, m)
    }
    fn create(&self, p: Ino, n: &str, m: u32) -> FsResult<Attr> {
        (**self).create(p, n, m)
    }
    fn symlink(&self, p: Ino, n: &str, t: &str) -> FsResult<Attr> {
        (**self).symlink(p, n, t)
    }
    fn readlink(&self, i: Ino) -> FsResult<String> {
        (**self).readlink(i)
    }
    fn link(&self, i: Ino, p: Ino, n: &str) -> FsResult<Attr> {
        (**self).link(i, p, n)
    }
    fn read(&self, i: Ino, o: u64, l: u32) -> FsResult<Vec<u8>> {
        (**self).read(i, o, l)
    }
    fn write(&self, i: Ino, o: u64, d: &[u8]) -> FsResult<usize> {
        (**self).write(i, o, d)
    }
    fn flush(&self, i: Ino) -> FsResult<()> {
        (**self).flush(i)
    }
    fn fsync(&self, i: Ino) -> FsResult<()> {
        (**self).fsync(i)
    }
    fn unlink(&self, p: Ino, n: &str) -> FsResult<()> {
        (**self).unlink(p, n)
    }
    fn rmdir(&self, p: Ino, n: &str) -> FsResult<()> {
        (**self).rmdir(p, n)
    }
    fn rename(&self, p: Ino, n: &str, np: Ino, nn: &str) -> FsResult<()> {
        (**self).rename(p, n, np, nn)
    }
    fn readdir(&self, i: Ino) -> FsResult<Vec<DirEntry>> {
        (**self).readdir(i)
    }
    fn setxattr(&self, i: Ino, n: &str, v: &[u8]) -> FsResult<()> {
        (**self).setxattr(i, n, v)
    }
    fn getxattr(&self, i: Ino, n: &str) -> FsResult<Vec<u8>> {
        (**self).getxattr(i, n)
    }
    fn listxattr(&self, i: Ino) -> FsResult<Vec<String>> {
        (**self).listxattr(i)
    }
    fn removexattr(&self, i: Ino, n: &str) -> FsResult<()> {
        (**self).removexattr(i, n)
    }
    fn stats(&self) -> FsResult<StoreStats> {
        (**self).stats()
    }
    fn space(&self) -> FsResult<Space> {
        (**self).space()
    }
}
