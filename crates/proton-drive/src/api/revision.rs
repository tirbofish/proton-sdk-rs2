use crate::api::ApiResponse;
use crate::api::block::BlockListingRevisionDto;
use crate::api::file::photos::PhotosAttributesDto;
use crate::api::file::thumbnail::{ThumbnailDto, ThumbnailDtoV2};
use crate::links::LinkId;
use crate::node::revision::RevisionState;
use crate::pgp::{PgpArmoredMessage, PgpArmoredSignature};
use crate::revision::RevisionId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ActiveRevisionDto {
    #[serde(rename = "RevisionID")]
    pub id: RevisionId,

    #[serde(rename = "CreateTime")]
    #[serde(deserialize_with = "crate::utils::serde::deserialize_time")]
    pub creation_time: chrono::DateTime<chrono::Utc>,

    #[serde(rename = "EncryptedSize")]
    pub storage_quota_consumption: i64,

    pub manifest_signature: Option<PgpArmoredSignature>,

    #[serde(rename = "XAttr")]
    pub extended_attributes: Option<PgpArmoredMessage>,

    pub thumbnails: Vec<ThumbnailDtoV2>,

    #[serde(rename = "SignatureEmail")]
    pub signature_email_address: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RevisionDto {
    #[serde(rename = "ID")]
    pub id: RevisionId,

    #[serde(rename = "ClientUID")]
    pub client_id: Option<String>,

    #[serde(rename = "CreateTime")]
    #[serde(with = "crate::utils::serde::epoch_seconds")]
    pub creation_time: DateTime<Utc>,

    pub size: i64,

    pub manifest_signature: Option<PgpArmoredSignature>,

    #[serde(rename = "SignatureEmail")]
    pub signature_email_address: Option<String>,

    pub state: RevisionState,

    #[serde(rename = "XAttr")]
    pub extended_attributes: Option<PgpArmoredMessage>,

    pub thumbnails: Option<Vec<ThumbnailDto>>,
}

#[derive(Debug, Deserialize)]
pub struct RevisionConflict {
    #[serde(rename = "ConflictLinkID")]
    pub link_id: Option<LinkId>,

    #[serde(rename = "ConflictRevisionID")]
    pub revision_id: Option<RevisionId>,

    #[serde(rename = "ConflictDraftRevisionID")]
    pub draft_revision_id: Option<RevisionId>,

    #[serde(rename = "ConflictDraftClientUID")]
    pub draft_client_uid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RevisionConflictResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    #[serde(rename = "Details")]
    pub conflict: RevisionConflict,
}

impl RevisionConflictResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Debug, Deserialize)]
pub struct RevisionCreationIdentity {
    #[serde(rename = "ID")]
    pub revision_id: RevisionId,
}

#[derive(Debug, Serialize)]
pub struct RevisionCreationRequest {
    #[serde(rename = "CurrentRevisionID")]
    pub current_revision_id: Option<RevisionId>,

    #[serde(rename = "ClientUID")]
    pub client_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RevisionCreationResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    #[serde(rename = "Revision")]
    pub identity: RevisionCreationIdentity,
}

impl RevisionCreationResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct RevisionUpdateRequest {
    pub manifest_signature: PgpArmoredSignature,

    #[serde(rename = "SignatureAddress")]
    pub signature_email_address: String,

    #[serde(rename = "XAttr")]
    pub extended_attributes: Option<PgpArmoredMessage>,

    #[serde(rename = "Photo")]
    pub photos_attributes: Option<PhotosAttributesDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RevisionResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    pub revision: BlockListingRevisionDto,
}

impl RevisionResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}
