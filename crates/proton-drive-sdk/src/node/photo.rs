use crate::node::NodeUid;
use crate::node::file::{DegradedFileNode, DegradedFileSecrets, FileNode, FileUploadMetadata};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde_repr::{Deserialize_repr, Serialize_repr};
use sha2::Sha256;
use std::collections::HashSet;

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

#[derive(Clone)]
pub(crate) struct PhotoUploadContext {
    pub metadata: PhotosFileUploadMetadata,
    pub hash_key: Vec<u8>,
}

impl PhotoUploadContext {
    pub fn attributes(
        &self,
        sha1: &[u8],
    ) -> anyhow::Result<crate::api::file::photos::PhotosAttributesDto> {
        let mut mac = <Hmac<Sha256> as hmac::digest::KeyInit>::new_from_slice(&self.hash_key)?;
        mac.update(hex::encode(sha1).as_bytes());
        Ok(crate::api::file::photos::PhotosAttributesDto {
            capture_time: self
                .metadata
                .capture_time
                .or(self.metadata.base.last_modification_time)
                .unwrap_or_else(Utc::now),
            content_hash_digest: mac.finalize().into_bytes().to_vec(),
            main_photo_link_id: self
                .metadata
                .main_photo_uid
                .as_ref()
                .map(|uid| uid.link_id.clone()),
            tags: Some(
                self.metadata
                    .tags
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<HashSet<_>>(),
            ),
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn photo_tags_use_typescript_api_values() {
        let cases = [
            (PhotoTag::Favorite, 0),
            (PhotoTag::Screenshot, 1),
            (PhotoTag::Video, 2),
            (PhotoTag::LivePhoto, 3),
            (PhotoTag::MotionPhoto, 4),
            (PhotoTag::Selfie, 5),
            (PhotoTag::Portrait, 6),
            (PhotoTag::Burst, 7),
            (PhotoTag::Panorama, 8),
            (PhotoTag::Raw, 9),
        ];
        for (tag, value) in cases {
            assert_eq!(serde_json::to_value(tag).unwrap(), value);
            assert_eq!(
                serde_json::from_value::<PhotoTag>(value.into()).unwrap(),
                tag
            );
        }
    }

    #[test]
    fn unknown_photo_tags_are_rejected() {
        assert!(serde_json::from_value::<PhotoTag>(10.into()).is_err());
        assert!(serde_json::from_value::<PhotoTag>((-1).into()).is_err());
    }

    #[test]
    fn photos_attributes_hash_sha1_hex_and_preserve_capture_time() {
        let capture_time = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let context = PhotoUploadContext {
            metadata: PhotosFileUploadMetadata {
                base: FileUploadMetadata {
                    last_modification_time: None,
                    additional_metadata: None,
                },
                capture_time: Some(capture_time),
                main_photo_uid: None,
                tags: Some(vec![PhotoTag::Favorite]),
            },
            hash_key: b"key".to_vec(),
        };
        let attributes = context.attributes(&[0x12; 20]).unwrap();
        assert_eq!(attributes.capture_time, capture_time);
        assert_eq!(
            hex::encode(&attributes.content_hash_digest),
            "79307225958a82ffbb477cac2080d60b3fd359722ccd4f957f6a33751794111f"
        );
        let json = serde_json::to_value(attributes).unwrap();
        assert_eq!(
            json["ContentHash"],
            "79307225958a82ffbb477cac2080d60b3fd359722ccd4f957f6a33751794111f"
        );
        assert_eq!(json["Tags"], serde_json::json!([0]));
        assert_eq!(json["CaptureTime"], 1_700_000_000);
    }

    #[test]
    fn photos_attributes_fall_back_to_modification_time() {
        let modified = Utc.timestamp_opt(1_710_000_000, 0).unwrap();
        let context = PhotoUploadContext {
            metadata: PhotosFileUploadMetadata {
                base: FileUploadMetadata {
                    last_modification_time: Some(modified),
                    additional_metadata: None,
                },
                capture_time: None,
                main_photo_uid: Some(NodeUid::from_parts("photos", "main")),
                tags: None,
            },
            hash_key: b"key".to_vec(),
        };
        let attributes = context.attributes(&[0; 20]).unwrap();
        assert_eq!(attributes.capture_time, modified);
        assert_eq!(attributes.main_photo_link_id.unwrap().raw(), "main");
        assert!(attributes.tags.unwrap().is_empty());
    }
}
