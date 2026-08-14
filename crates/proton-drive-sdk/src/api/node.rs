use crate::api::ApiResponse;
use crate::api::links::NameHashDigestUnavailabilityDto;
use crate::links::LinkId;
use crate::pgp::{PgpArmoredMessage, PgpArmoredPrivateKey, PgpArmoredSignature};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct NodeCreationRequest {
    #[serde(rename = "Name")]
    pub name: PgpArmoredMessage,

    #[serde(rename = "Hash")]
    #[serde(with = "crate::utils::serde::forgiving_hex_bytes")]
    pub name_hash_digest: Vec<u8>,

    #[serde(rename = "ParentLinkID")]
    pub parent_link_id: LinkId,

    #[serde(rename = "NodePassphrase")]
    pub passphrase: PgpArmoredMessage,

    #[serde(rename = "NodePassphraseSignature")]
    pub passphrase_signature: PgpArmoredSignature,

    #[serde(rename = "NodeKey")]
    pub key: PgpArmoredPrivateKey,
}

#[derive(Debug, Serialize)]
pub struct NodeNameAvailabilityRequest {
    #[serde(rename = "Hashes")]
    pub name_hash_digests: Vec<String>,

    #[serde(rename = "ClientUID")]
    pub client_uid: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct NodeNameAvailabilityResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    #[serde(rename = "AvailableHashes")]
    pub available_name_hash_digests: Vec<String>,

    #[serde(rename = "PendingHashes")]
    pub unavailable_name_hash_digests: Vec<NameHashDigestUnavailabilityDto>,
}

impl NodeNameAvailabilityResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}
