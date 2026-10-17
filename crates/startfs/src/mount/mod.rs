//! The mount layer: thin adapters that present a [`FsOps`] to the operating
//! system as a real filesystem.
//!
//! Everything platform-independent (attribute and errno translation) lives in
//! [`translate`]; the FUSE adapter in [`fuse`] only forwards kernel calls.
//! A future WinFsp adapter implements [`Mounter`] too, so the daemon never
//! names a concrete platform.

#[cfg(unix)]
pub mod fuse;
#[cfg(unix)]
pub mod translate;

use crate::error::Error;
use startfs_core::ops::FsOps;
use std::path::Path;
use std::sync::Arc;

/// Something that can present a [`FsOps`] at a mount point.
pub trait Mounter {
    /// Mount `fs` at `mountpoint` and return at once; the mount lives until
    /// the returned handle is unmounted or dropped.
    fn mount(&self, fs: Arc<dyn FsOps>, mountpoint: &Path) -> Result<Box<dyn Mount>, Error>;
}

/// A live mount.
pub trait Mount: Send {
    fn unmount(self: Box<Self>) -> Result<(), Error>;
}

/// The mounter for this platform, if there is one. Windows (WinFsp) would add
/// a `Mounter` here.
pub fn platform_mounter() -> Option<Box<dyn Mounter>> {
    #[cfg(unix)]
    return Some(Box::new(fuse::FuseMounter));
    #[cfg(not(unix))]
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::translate::*;
    use startfs_core::ops::*;

    #[test]
    fn attributes_convert_to_fuse_attributes() {
        let a = Attr {
            ino: 7,
            kind: FileKind::File,
            mode: 0o640,
            nlink: 1,
            uid: 1000,
            gid: 100,
            size: 5000,
            mtime: 2_500_000_000,
            ctime: 3_000_000_001,
        };
        let f = to_fuse_attr(&a);
        assert_eq!(f.ino.0, 7);
        assert_eq!(f.kind, fuser::FileType::RegularFile);
        assert_eq!(f.perm, 0o640);
        assert_eq!((f.uid, f.gid, f.nlink, f.size), (1000, 100, 1, 5000));
        assert_eq!(f.blocks, 10); // 5000 bytes in 512-byte units
        assert_eq!(
            f.mtime,
            std::time::UNIX_EPOCH + std::time::Duration::new(2, 500_000_000)
        );
        assert_eq!(
            f.ctime,
            std::time::UNIX_EPOCH + std::time::Duration::new(3, 1)
        );
        assert_eq!(kind_to_fuse(FileKind::Dir), fuser::FileType::Directory);
        assert_eq!(kind_to_fuse(FileKind::Symlink), fuser::FileType::Symlink);
    }

    #[test]
    fn errors_map_to_errno() {
        assert_eq!(to_errno(&FsError::NotFound), fuser::Errno::ENOENT);
        assert_eq!(to_errno(&FsError::NoSpace).code(), libc::ENOSPC);
        assert_eq!(to_errno(&FsError::Io("x".into())).code(), libc::EIO);
    }

    #[test]
    fn times_convert_to_nanoseconds() {
        use std::time::{Duration, UNIX_EPOCH};
        let t = fuser::TimeOrNow::SpecificTime(UNIX_EPOCH + Duration::new(4, 7));
        assert_eq!(time_to_nanos(t, 99), 4_000_000_007);
        assert_eq!(time_to_nanos(fuser::TimeOrNow::Now, 99), 99);
    }
}
