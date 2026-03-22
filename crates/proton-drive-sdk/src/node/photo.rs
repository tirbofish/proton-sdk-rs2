use crate::node::NodeUid;
use crate::node::file::{DegradedFileNode, DegradedFileSecrets, FileNode, FileUploadMetadata};
use chrono::{DateTime, Utc};
use serde_repr::{Deserialize_repr, Serialize_repr};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DegradedPhotoNode {
    #[serde(flatten)]
    pub file: DegradedFileNode,
    pub capture_time: DateTime<Utc>,
    pub album_uids: Vec<NodeUid>,
}

#[derive(Debug, Clone)]
pub struct DegradedPhotoNodeMetadata {
    pub node: DegradedPhotoNode,
    pub secrets: DegradedFileSecrets,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PhotoNode {
    #[serde(flatten)]
    pub base: FileNode,
    pub capture_time: DateTime<Utc>,
    pub album_uids: Vec<NodeUid>,
}

#[derive(Debug, Clone)]
pub struct PhotosFileUploadMetadata {
    pub base: FileUploadMetadata,
    pub capture_time: Option<DateTime<Utc>>,
    pub main_photo_uid: Option<NodeUid>,
    pub tags: Option<Vec<PhotoTag>>,
}

#[derive(Debug, Clone)]
pub struct PhotosTimelineItem {
    pub uid: NodeUid,
    pub capture_time: DateTime<Utc>,
}

/// A single entry from the timeline API page response — includes tags.
#[derive(Debug, Clone)]
pub struct TimelineEntry {
    pub uid: NodeUid,
    pub capture_time: DateTime<Utc>,
    pub tags: Vec<PhotoTag>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum PhotoTag {
    Favorite = 0,
    Screenshot = 1,
    Video = 2,
    LivePhoto = 3,
    MotionPhoto = 4,
    Selfie = 5,
    Portrait = 6,
    Burst = 7,
    Panorama = 8,
    Raw = 9,
}
