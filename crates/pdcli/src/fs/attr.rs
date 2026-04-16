use std::time::SystemTime;

use fuse3::raw::prelude::*;

use crate::index::store::{IndexEntry, ROOT_INO};

const DIR_PERM: u16 = 0o755;
const FILE_PERM: u16 = 0o644;

/// Build a `FileAttr` from an `IndexEntry`.
pub fn make_attr(ino: u64, entry: &IndexEntry) -> FileAttr {
    let time = if entry.mtime > 0 {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(entry.mtime as u64)
    } else {
        SystemTime::now()
    };

    FileAttr {
        ino,
        size: entry.size,
        blocks: (entry.size + 511) / 512,
        atime: time.into(),
        mtime: time.into(),
        ctime: time.into(),
        #[cfg(target_os = "macos")]
        crtime: time.into(),
        kind: if entry.is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        },
        perm: if entry.is_dir { DIR_PERM } else { FILE_PERM },
        nlink: if entry.is_dir { 2 } else { 1 },
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        #[cfg(target_os = "macos")]
        flags: 0,
        blksize: 4096,
    }
}

/// `FileAttr` for the FUSE root directory (inode 1).
pub fn root_attr() -> FileAttr {
    FileAttr {
        ino: ROOT_INO,
        size: 0,
        blocks: 0,
        atime: SystemTime::now().into(),
        mtime: SystemTime::now().into(),
        ctime: SystemTime::now().into(),
        #[cfg(target_os = "macos")]
        crtime: SystemTime::now().into(),
        kind: FileType::Directory,
        perm: DIR_PERM,
        nlink: 2,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        #[cfg(target_os = "macos")]
        flags: 0,
        blksize: 4096,
    }
}
