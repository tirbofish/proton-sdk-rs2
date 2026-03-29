use crate::api::file::FileContentDigestsDto;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CommonExtendedAttributes {
    /// Plaintext file size in bytes as claimed by the uploader.
    pub size: Option<i64>,

    #[serde(default, with = "crate::utils::serde::iso8601_opt")]
    /// Last-modified timestamp claimed by the client at upload time.
    pub modification_time: Option<DateTime<Utc>>,

    /// Sizes of each encrypted block, in the same order as the block list.
    pub block_sizes: Option<Vec<i32>>,

    /// Content digests (e.g. SHA-1) for integrity verification after decryption.
    pub digests: Option<FileContentDigestsDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaExtendedAttributes {
    /// Width of the media file in pixels.
    pub width: Option<u32>,
    /// Height of the media file in pixels.
    pub height: Option<u32>,
    /// Duration of the media file in seconds (for video/audio).
    pub duration: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ExtendedAttributes {
    /// Common file attributes shared by all node types (size, modification time, digests).
    pub common: Option<CommonExtendedAttributes>,

    #[serde(rename = "Media")]
    /// Media-specific attributes present for image and video files.
    pub media: Option<MediaExtendedAttributes>,

    #[serde(flatten)]
    /// Any extra vendor or future attributes not yet known to this SDK version.
    pub additional_metadata: HashMap<String, Value>,
}
