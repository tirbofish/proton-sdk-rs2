use crate::account::AddressId;
use crate::api::ApiResponse;
use crate::api::block::verification::BlockVerificationOutput;
use crate::api::file::thumbnail::{ThumbnailBlockUploadTarget, ThumbnailCreationRequest};
use crate::api::revision::RevisionDto;
use crate::links::LinkId;
use crate::pgp::PgpArmoredMessage;
use crate::revision::RevisionId;
use crate::volume::VolumeId;
use serde::{Deserialize, Serialize};

pub mod verification;

#[derive(Debug, Serialize)]
pub struct BlockCreationRequest {
    #[serde(rename = "Index")]
    pub index: i32,

    #[serde(rename = "Size")]
    pub size: i32,

    #[serde(rename = "EncSignature")]
    pub encrypted_signature: PgpArmoredMessage,

    #[serde(rename = "Hash", with = "crate::utils::serde::base64_bytes")]
    pub hash_digest: Vec<u8>,

    #[serde(rename = "Verifier")]
    pub verification_output: BlockVerificationOutput,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BlockDto {
    pub index: i32,

    #[serde(rename = "Hash")]
    #[serde(default)]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub hash_digest: Vec<u8>,

    #[serde(rename = "BareURL")]
    #[serde(default)]
    #[serde(deserialize_with = "crate::utils::serde::deserialize_null_default")]
    pub bare_url: String,

    #[serde(default)]
    #[serde(deserialize_with = "crate::utils::serde::deserialize_null_default")]
    pub token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BlockListingRevisionDto {
    #[serde(flatten)]
    pub revision: RevisionDto,

    #[serde(default)]
    pub blocks: Vec<BlockDto>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct BlockUploadPreparationRequest {
    #[serde(rename = "AddressID")]
    pub address_id: AddressId,

    #[serde(rename = "VolumeID")]
    pub volume_id: VolumeId,

    #[serde(rename = "LinkID")]
    pub link_id: LinkId,

    #[serde(rename = "RevisionID")]
    pub revision_id: RevisionId,

    #[serde(rename = "BlockList")]
    pub blocks: Vec<BlockCreationRequest>,

    #[serde(rename = "ThumbnailList")]
    pub thumbnails: Vec<ThumbnailCreationRequest>,
}

#[derive(Debug, Deserialize)]
pub struct BlockUploadPreparationResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    #[serde(rename = "UploadLinks")]
    pub upload_targets: Vec<BlockUploadTarget>,

    #[serde(rename = "ThumbnailLinks")]
    pub thumbnail_upload_targets: Vec<ThumbnailBlockUploadTarget>,
}

impl BlockUploadPreparationResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BlockUploadTarget {
    #[serde(rename = "BareURL")]
    pub bare_url: String,

    pub token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BlockUploadUrl {
    pub token: String,

    #[serde(rename = "URL")]
    pub value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BlockVerificationInputResponse {
    #[serde(with = "crate::utils::serde::base64_bytes")]
    pub verification_code: Vec<u8>,

    #[serde(with = "crate::utils::serde::base64_bytes")]
    pub content_key_packet: Vec<u8>,
}
