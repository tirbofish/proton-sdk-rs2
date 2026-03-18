use std::collections::HashMap;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use crate::api::file::FileContentDigestsDto;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CommonExtendedAttributes {
    pub size: Option<i64>,

    #[serde(default, with = "crate::utils::serde::epoch_seconds_opt")]
    pub modification_time: Option<DateTime<Utc>>,

    pub block_sizes: Option<Vec<i32>>,

    pub digests: Option<FileContentDigestsDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ExtendedAttributes {
    pub common: Option<CommonExtendedAttributes>,

    #[serde(flatten)]
    pub additional_metadata: HashMap<String, Value>,
}