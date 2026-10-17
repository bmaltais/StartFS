//! Pure translation between [`startfs_core::ops`] types and FUSE types.

use fuser::{Errno, FileAttr, FileType, INodeNo, TimeOrNow};
use startfs_core::ops::{Attr, FileKind, FsError, Nanos};
use std::time::{Duration, UNIX_EPOCH};

pub fn kind_to_fuse(kind: FileKind) -> FileType {
    match kind {
        FileKind::File => FileType::RegularFile,
        FileKind::Dir => FileType::Directory,
        FileKind::Symlink => FileType::Symlink,
    }
}

fn time(n: Nanos) -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_nanos(n)
}

pub fn to_fuse_attr(a: &Attr) -> FileAttr {
    FileAttr {
        ino: INodeNo(a.ino),
        size: a.size,
        blocks: a.size.div_ceil(512),
        atime: time(a.mtime),
        mtime: time(a.mtime),
        ctime: time(a.ctime),
        crtime: time(a.ctime),
        kind: kind_to_fuse(a.kind),
        perm: (a.mode & 0o7777) as u16,
        nlink: a.nlink,
        uid: a.uid,
        gid: a.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

pub fn to_errno(e: &FsError) -> Errno {
    Errno::from_i32(e.errno())
}

/// A requested time as nanoseconds; "now" is resolved with the caller's clock.
pub fn time_to_nanos(t: TimeOrNow, now: Nanos) -> Nanos {
    match t {
        TimeOrNow::Now => now,
        TimeOrNow::SpecificTime(t) => t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as Nanos)
            .unwrap_or(0),
    }
}
