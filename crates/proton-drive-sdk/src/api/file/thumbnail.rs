use crate::api::block::BlockUploadTarget;
use crate::node::thumbnail::ThumbnailType;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct ThumbnailBlock {
    #[serde(rename = "ThumbnailID")]
    pub thumbnail_id: String,

    #[serde(rename = "BareURL")]
    pub bare_url: String,

    #[serde(rename = "Token")]
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct ThumbnailBlockListRequest {
    #[serde(rename = "ThumbnailIDs")]
    pub thumbnail_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ThumbnailBlockListResponse {
    #[serde(flatten)]
    pub base: crate::api::ApiResponse,

    #[serde(rename = "Thumbnails")]
    pub blocks: Vec<ThumbnailBlock>,
}

impl ThumbnailBlockListResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Debug, Deserialize)]
pub struct ThumbnailBlockUploadTarget {
    #[serde(flatten)]
    pub base: BlockUploadTarget,

    #[serde(rename = "ThumbnailType")]
    pub r#type: ThumbnailType,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ThumbnailCreationRequest {
    pub size: i32,

    pub r#type: ThumbnailType,

    #[serde(rename = "Hash")]
    #[serde(with = "crate::utils::serde::base64_bytes")]
    pub hash_digest: Vec<u8>,
}

#[derive(Debug, Deserialize)]
pub struct ThumbnailDto {
    #[serde(rename = "ThumbnailID")]
    pub id: String,

    #[serde(rename = "Type")]
    pub r#type: ThumbnailType,

    #[serde(rename = "Hash")]
    #[serde(with = "crate::utils::serde::base64_bytes")]
    pub hash_digest: Vec<u8>,

    #[serde(rename = "Size")]
    pub size_on_cloud_storage: i32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ThumbnailDtoV2 {
    #[serde(rename = "ThumbnailID")]
    pub id: String,

    #[serde(rename = "Type")]
    pub r#type: ThumbnailType,

    #[serde(rename = "Hash")]
    #[serde(with = "crate::utils::serde::base64_bytes")]
    pub hash_digest: Vec<u8>,

    #[serde(rename = "EncryptedSize")]
    pub storage_quota_usage: i32,
}
