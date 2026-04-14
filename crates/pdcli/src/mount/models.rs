use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use fuse3::raw::prelude::{FileAttr, FileType};

use proton_drive_sdk::node::file::FileNode as DriveFileNode;
use proton_drive_sdk::node::folder::FolderNode as DriveFolderNode;
use proton_drive_sdk::node::revision::RevisionUid;
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::utils::PotentialObject;

use super::{DIR_MODE, FILE_MODE};

/// Convert chrono DateTime to SystemTime.
pub(super) fn datetime_to_system_time(dt: DateTime<Utc>) -> SystemTime {
    let timestamp = dt.timestamp();
    if timestamp >= 0 {
        UNIX_EPOCH + Duration::from_secs(timestamp as u64)
    } else {
        UNIX_EPOCH
    }
}

/// Metadata for a Proton Drive file.
#[derive(Debug, Clone)]
#[allow(dead_code)] // temporary for now
pub struct ProtonFileMetadata {
    pub uid: NodeUid,
    pub parent_uid: Option<NodeUid>,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub size_on_cloud: u64,
    pub creation_time: DateTime<Utc>,
    pub modification_time: Option<DateTime<Utc>>,
    pub trash_time: Option<DateTime<Utc>>,
    pub author_email: Option<String>,
    pub name_author_email: Option<String>,
    pub owner_email: Option<String>,
    pub owner_organisation: Option<String>,
    pub revision_uid: RevisionUid,
    pub revision_creation_time: DateTime<Utc>,
    pub content_sha1: Option<Vec<u8>>,
    pub is_photo: bool,
    pub capture_time: Option<DateTime<Utc>>,
    pub thumbnail_id: Option<String>,
}

impl ProtonFileMetadata {
    pub fn from_file_node(node: &DriveFileNode, is_photo: bool, capture_time: Option<DateTime<Utc>>) -> Self {
        let revision = &node.active_revision;

        Self {
            uid: node.base.base.uid.clone(),
            parent_uid: node.base.base.parent_uid.clone(),
            name: node.base.base.name.clone(),
            mime_type: node.base.media_type.clone(),
            size: revision.claimed_size.unwrap_or(0) as u64,
            size_on_cloud: node.total_size_on_cloud_storage as u64,
            creation_time: node.base.base.creation_time,
            modification_time: revision.claimed_modification_time,
            trash_time: node.base.base.trash_time,
            author_email: node.base.base.author.as_ref().ok().and_then(|a| a.email_address.clone()),
            name_author_email: node.base.base.name_author.as_ref().ok().and_then(|a| a.email_address.clone()),
            owner_email: node.base.base.owned_by.as_ref().and_then(|o| o.email.clone()),
            owner_organisation: node.base.base.owned_by.as_ref().and_then(|o| o.organisation.clone()),
            revision_uid: revision.uid.clone(),
            revision_creation_time: revision.creation_time,
            content_sha1: revision.claimed_digests.sha1.clone(),
            is_photo,
            capture_time,
            thumbnail_id: {
                let thumb = revision
                    .thumbnails
                    .iter()
                    .find(|t| t.r#type == 1)
                    .or_else(|| revision.thumbnails.first());
                if thumb.is_some() {
                    tracing::debug!(
                        "File '{}' has {} thumbnail(s), using {:?}",
                        node.base.base.name,
                        revision.thumbnails.len(),
                        thumb.map(|t| &t.id)
                    );
                }
                thumb.map(|t| t.id.clone())
            },
        }
    }
}

/// Metadata for a Proton Drive folder.
#[derive(Debug, Clone)]
#[allow(dead_code)] // temporary for now
pub struct ProtonFolderMetadata {
    pub uid: NodeUid,
    pub parent_uid: Option<NodeUid>,
    pub name: String,
    pub creation_time: DateTime<Utc>,
    pub trash_time: Option<DateTime<Utc>>,
    /// Bumped to `Utc::now()` whenever children are added/removed/updated
    /// so that Nautilus detects the change and refreshes.
    pub content_modified_at: Option<DateTime<Utc>>,
    pub author_email: Option<String>,
    pub name_author_email: Option<String>,
    pub owner_email: Option<String>,
    pub owner_organisation: Option<String>,
    pub is_album: bool,
}

impl ProtonFolderMetadata {
    pub fn from_folder_node(node: &DriveFolderNode, is_album: bool) -> Self {
        Self {
            uid: node.base.uid.clone(),
            parent_uid: node.base.parent_uid.clone(),
            name: node.base.name.clone(),
            creation_time: node.base.creation_time,
            trash_time: node.base.trash_time,
            author_email: node.base.author.as_ref().ok().and_then(|a| a.email_address.clone()),
            name_author_email: node.base.name_author.as_ref().ok().and_then(|a| a.email_address.clone()),
            owner_email: node.base.owned_by.as_ref().and_then(|o| o.email.clone()),
            owner_organisation: node.base.owned_by.as_ref().and_then(|o| o.organisation.clone()),
            is_album,
            content_modified_at: None,
        }
    }
}

/// Metadata for a degraded node (decryption failed for some fields).
#[derive(Debug, Clone)]
#[allow(dead_code)] // temporary for now
pub struct DegradedNodeMetadata {
    pub uid: NodeUid,
    pub parent_uid: Option<NodeUid>,
    pub name: String,
    pub is_file: bool,
    pub mime_type: Option<String>,
    pub size_on_cloud: Option<u64>,
    pub creation_time: DateTime<Utc>,
    pub errors: Vec<String>,
}

/// A node in the filesystem (either file or directory).
#[derive(Debug, Clone)]
pub enum FsNode {
    File(ProtonFileMetadata),
    Folder(ProtonFolderMetadata),
    Degraded(DegradedNodeMetadata),
}

impl FsNode {
    pub fn uid(&self) -> &NodeUid {
        match self {
            FsNode::File(f) => &f.uid,
            FsNode::Folder(f) => &f.uid,
            FsNode::Degraded(d) => &d.uid,
        }
    }

    pub fn parent_uid(&self) -> Option<&NodeUid> {
        match self {
            FsNode::File(f) => f.parent_uid.as_ref(),
            FsNode::Folder(f) => f.parent_uid.as_ref(),
            FsNode::Degraded(d) => d.parent_uid.as_ref(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            FsNode::File(f) => &f.name,
            FsNode::Folder(f) => &f.name,
            FsNode::Degraded(d) => &d.name,
        }
    }

    pub fn is_dir(&self) -> bool {
        match self {
            FsNode::File(_) => false,
            FsNode::Folder(_) => true,
            FsNode::Degraded(d) => !d.is_file,
        }
    }

    pub fn file_type(&self) -> FileType {
        if self.is_dir() {
            FileType::Directory
        } else {
            FileType::RegularFile
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            FsNode::File(f) => f.size,
            FsNode::Folder(_) => 0,
            FsNode::Degraded(d) => d.size_on_cloud.unwrap_or(0),
        }
    }

    pub fn creation_time(&self) -> SystemTime {
        let dt = match self {
            FsNode::File(f) => f.creation_time,
            FsNode::Folder(f) => f.creation_time,
            FsNode::Degraded(d) => d.creation_time,
        };
        datetime_to_system_time(dt)
    }

    pub fn modification_time(&self) -> SystemTime {
        match self {
            FsNode::File(f) => {
                if let Some(mtime) = f.modification_time {
                    datetime_to_system_time(mtime)
                } else {
                    datetime_to_system_time(f.creation_time)
                }
            }
            FsNode::Folder(f) => {
                if let Some(mtime) = f.content_modified_at {
                    datetime_to_system_time(mtime)
                } else {
                    datetime_to_system_time(f.creation_time)
                }
            }
            FsNode::Degraded(d) => datetime_to_system_time(d.creation_time),
        }
    }

    pub fn attr(&self, inode: u64) -> FileAttr {
        let size = self.size();
        let ctime = self.creation_time();
        let mtime = self.modification_time();
        let perm = if self.is_dir() { DIR_MODE } else { FILE_MODE };
        let nlink = if self.is_dir() { 2 } else { 1 };

        FileAttr {
            ino: inode,
            size,
            blocks: (size + 4095) / 4096,
            atime: mtime.into(),
            mtime: mtime.into(),
            ctime: ctime.into(),
            kind: self.file_type(),
            perm,
            nlink,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
        }
    }
}
