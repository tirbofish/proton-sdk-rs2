use crate::api::file::FileContentDigestsDto;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CommonExtendedAttributes {
    /// Plaintext file size in bytes as claimed by the uploader.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, with = "crate::utils::serde::iso8601_opt")]
    /// Last-modified timestamp claimed by the client at upload time.
    pub modification_time: Option<DateTime<Utc>>,

    /// Sizes of each encrypted block, in the same order as the block list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_sizes: Option<Vec<i32>>,

    /// Content digests (e.g. SHA-1) for integrity verification after decryption.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digests: Option<FileContentDigestsDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaExtendedAttributes {
    /// Width of the media file in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Height of the media file in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Duration of the media file in seconds (for video/audio).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ExtendedAttributes {
    /// Common file attributes shared by all node types (size, modification time, digests).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub common: Option<CommonExtendedAttributes>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "Media")]
    /// Media-specific attributes present for image and video files.
    pub media: Option<MediaExtendedAttributes>,

    #[serde(flatten)]
    /// Any extra vendor or future attributes not yet known to this SDK version.
    pub additional_metadata: HashMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::file::FileContentDigestsDto;
    use chrono::TimeZone;
    use serde_json::json;

    #[test]
    fn serializes_empty_attributes_without_null_fields() {
        let attributes = ExtendedAttributes {
            common: None,
            media: None,
            additional_metadata: HashMap::new(),
        };
        assert_eq!(serde_json::to_value(attributes).unwrap(), json!({}));
    }

    #[test]
    fn serializes_typescript_file_attribute_shape() {
        let attributes = ExtendedAttributes {
            common: Some(CommonExtendedAttributes {
                size: Some(1234),
                modification_time: Some(Utc.timestamp_opt(1_234_567_890, 0).unwrap()),
                block_sizes: Some(vec![1200, 34]),
                digests: Some(FileContentDigestsDto {
                    sha1: Some(vec![0xab, 0xcd, 0xef]),
                }),
            }),
            media: None,
            additional_metadata: HashMap::new(),
        };
        assert_eq!(
            serde_json::to_value(attributes).unwrap(),
            json!({
                "Common": {
                    "ModificationTime": "2009-02-13T23:31:30.000Z",
                    "Size": 1234,
                    "BlockSizes": [1200, 34],
                    "Digests": {"SHA1": "abcdef"}
                }
            })
        );
    }

    #[test]
    fn preserves_media_and_unknown_metadata() {
        let attributes: ExtendedAttributes = serde_json::from_value(json!({
            "Common": {},
            "Media": {"Width": 100, "Height": 200},
            "Camera": {"Make": "Example"}
        }))
        .unwrap();

        let media = attributes.media.unwrap();
        assert_eq!(
            (media.width, media.height, media.duration),
            (Some(100), Some(200), None)
        );
        assert_eq!(
            attributes.additional_metadata.get("Camera"),
            Some(&json!({"Make": "Example"}))
        );
    }

    #[test]
    fn parses_claimed_block_sizes_and_digest() {
        let attributes: ExtendedAttributes = serde_json::from_value(json!({
            "Common": {
                "BlockSizes": [1024, 1024, 123],
                "Digests": {"SHA1": "abcdef"}
            }
        }))
        .unwrap();
        let common = attributes.common.unwrap();
        assert_eq!(common.block_sizes, Some(vec![1024, 1024, 123]));
        assert_eq!(common.digests.unwrap().sha1, Some(vec![0xab, 0xcd, 0xef]));
    }

    #[test]
    fn rejects_invalid_timestamp_instead_of_using_it() {
        assert!(
            serde_json::from_value::<ExtendedAttributes>(
                json!({"Common": {"ModificationTime": "invalid"}})
            )
            .is_err()
        );
    }
}
