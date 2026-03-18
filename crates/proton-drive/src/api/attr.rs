use crate::api::file::FileContentDigestsDto;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CommonExtendedAttributes {
    pub size: Option<i64>,

    #[serde(default, with = "crate::utils::serde::iso8601_opt")]
    pub modification_time: Option<DateTime<Utc>>,

    pub block_sizes: Option<Vec<i32>>,

    pub digests: Option<FileContentDigestsDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaExtendedAttributes {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ExtendedAttributes {
    pub common: Option<CommonExtendedAttributes>,

    #[serde(rename = "Media")]
    pub media: Option<MediaExtendedAttributes>,

    #[serde(flatten)]
    pub additional_metadata: HashMap<String, Value>,
}
