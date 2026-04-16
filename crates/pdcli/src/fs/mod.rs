mod attr;

use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fuse3::raw::prelude::*;
use fuse3::Result;
use futures_util::stream;

use crate::index::store::ROOT_INO;
use crate::index::DriveIndex;

const TTL: Duration = Duration::from_secs(1);

const STATFS: ReplyStatFs = ReplyStatFs {
    blocks: 1 << 20,
    bfree: 1 << 19,
    bavail: 1 << 19,
    files: 1 << 16,
    ffree: 1 << 15,
    bsize: 4096,
    namelen: u32::MAX,
    frsize: 0,
};

pub struct ProtonDriveFS {
    index: Arc<DriveIndex>,
}

impl ProtonDriveFS {
    pub fn new(index: Arc<DriveIndex>) -> Self {
        Self { index }
    }
}

impl fuse3::raw::Filesystem for ProtonDriveFS {
    async fn init(&self, _req: Request) -> Result<ReplyInit> {
        Ok(ReplyInit {
            max_write: NonZeroU32::new(16 * 1024).unwrap(),
        })
    }

    async fn destroy(&self, _req: Request) {}

    async fn lookup(&self, _req: Request, parent: u64, name: &OsStr) -> Result<ReplyEntry> {
        let name_str = name.to_str().ok_or_else(|| fuse3::Errno::from(libc::ENOENT))?;

        // First try the cached lookup
        if let Some(ino) = self.index.lookup(parent, name_str).await {
            if let Some(entry) = self.index.get_node(ino).await {
                return Ok(ReplyEntry {
                    ttl: TTL,
                    attr: attr::make_attr(ino, &entry),
                    generation: 0,
                });
            }
        }

        // If not found, try fetching children (triggers online fetch if needed)
        let children = self.index.children(parent).await;
        for (ino, entry) in &children {
            if entry.name == name_str {
                return Ok(ReplyEntry {
                    ttl: TTL,
                    attr: attr::make_attr(*ino, entry),
                    generation: 0,
                });
            }
        }

        Err(libc::ENOENT.into())
    }

    async fn getattr(
        &self,
        _req: Request,
        inode: u64,
        _fh: Option<u64>,
        _flags: u32,
    ) -> Result<ReplyAttr> {
        if inode == ROOT_INO {
            return Ok(ReplyAttr {
                ttl: TTL,
                attr: attr::root_attr(),
            });
        }

        let entry = self.index.get_node(inode).await
            .ok_or_else(|| fuse3::Errno::from(libc::ENOENT))?;

        Ok(ReplyAttr {
            ttl: TTL,
            attr: attr::make_attr(inode, &entry),
        })
    }

    async fn open(&self, _req: Request, inode: u64, flags: u32) -> Result<ReplyOpen> {
        // Verify inode exists
        if inode != ROOT_INO {
            self.index.get_node(inode).await
                .ok_or_else(|| fuse3::Errno::from(libc::ENOENT))?;
        }

        Ok(ReplyOpen { fh: 0, flags })
    }

    async fn read(
        &self,
        _req: Request,
        inode: u64,
        _fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<ReplyData> {
        match self.index.read_file(inode, offset, size).await {
            Some(data) => Ok(ReplyData {
                data: Bytes::from(data),
            }),
            None => Ok(ReplyData { data: Bytes::new() }),
        }
    }

    async fn readdir(
        &self,
        _req: Request,
        inode: u64,
        _fh: u64,
        offset: i64,
    ) -> Result<ReplyDirectory<impl futures_util::Stream<Item = Result<DirectoryEntry>> + Send + '_>>
    {
        let children = self.index.children(inode).await;

        let mut entries = vec![
            Ok(DirectoryEntry {
                inode,
                kind: FileType::Directory,
                name: OsString::from("."),
                offset: 1,
            }),
            Ok(DirectoryEntry {
                inode: self.index.get_node(inode).await
                    .and_then(|_| {
                        let store = self.index.store.try_read().ok()?;
                        store.parent_ino(inode)
                    })
                    .unwrap_or(ROOT_INO),
                kind: FileType::Directory,
                name: OsString::from(".."),
                offset: 2,
            }),
        ];

        for (i, (child_ino, child_entry)) in children.iter().enumerate() {
            entries.push(Ok(DirectoryEntry {
                inode: *child_ino,
                kind: if child_entry.is_dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                },
                name: OsString::from(&child_entry.name),
                offset: (i + 3) as i64,
            }));
        }

        Ok(ReplyDirectory {
            entries: stream::iter(entries.into_iter().skip(offset as usize)),
        })
    }

    async fn access(&self, _req: Request, inode: u64, _mask: u32) -> Result<()> {
        if inode == ROOT_INO {
            return Ok(());
        }
        self.index.get_node(inode).await
            .ok_or_else(|| fuse3::Errno::from(libc::ENOENT))?;
        Ok(())
    }

    async fn readdirplus(
        &self,
        _req: Request,
        parent: u64,
        _fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> Result<
        ReplyDirectoryPlus<impl futures_util::Stream<Item = Result<DirectoryEntryPlus>> + Send + '_>,
    > {
        let children = self.index.children(parent).await;

        let parent_attr = if parent == ROOT_INO {
            attr::root_attr()
        } else {
            self.index.get_node(parent).await
                .map(|e| attr::make_attr(parent, &e))
                .unwrap_or_else(|| attr::root_attr())
        };

        let parent_parent = self.index.get_node(parent).await
            .and_then(|_| {
                let store = self.index.store.try_read().ok()?;
                store.parent_ino(parent)
            })
            .unwrap_or(ROOT_INO);

        let mut entries = vec![
            Ok(DirectoryEntryPlus {
                inode: parent,
                generation: 0,
                kind: FileType::Directory,
                name: OsString::from("."),
                offset: 1,
                attr: parent_attr,
                entry_ttl: TTL,
                attr_ttl: TTL,
            }),
            Ok(DirectoryEntryPlus {
                inode: parent_parent,
                generation: 0,
                kind: FileType::Directory,
                name: OsString::from(".."),
                offset: 2,
                attr: if parent_parent == ROOT_INO {
                    attr::root_attr()
                } else {
                    self.index.get_node(parent_parent).await
                        .map(|e| attr::make_attr(parent_parent, &e))
                        .unwrap_or_else(|| attr::root_attr())
                },
                entry_ttl: TTL,
                attr_ttl: TTL,
            }),
        ];

        for (i, (child_ino, child_entry)) in children.iter().enumerate() {
            entries.push(Ok(DirectoryEntryPlus {
                inode: *child_ino,
                generation: 0,
                kind: if child_entry.is_dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                },
                name: OsString::from(&child_entry.name),
                offset: (i + 3) as i64,
                attr: attr::make_attr(*child_ino, child_entry),
                entry_ttl: TTL,
                attr_ttl: TTL,
            }));
        }

        Ok(ReplyDirectoryPlus {
            entries: stream::iter(entries.into_iter().skip(offset as usize)),
        })
    }

    async fn statfs(&self, _req: Request, _inode: u64) -> Result<ReplyStatFs> {
        Ok(STATFS)
    }
}
