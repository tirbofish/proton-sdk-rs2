use crate::api::ApiResponse;
use crate::api::links::NameHashDigestUnavailabilityDto;
use crate::links::LinkId;
use crate::node::NodeUid;
use crate::pgp::{PgpArmoredMessage, PgpArmoredPrivateKey, PgpArmoredSignature};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A node opened or previewed by the user. An omitted access time defaults to now.
#[derive(Debug, Clone)]
pub struct RecentlyAccessedItem {
    pub node_uid: NodeUid,
    pub access_time: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct RecentlyAccessedRequest {
    pub recently_accessed_items: Vec<RecentlyAccessedDto>,
}

#[derive(Serialize)]
pub(crate) struct RecentlyAccessedDto {
    #[serde(rename = "VolumeID")]
    volume_id: crate::volume::VolumeId,
    #[serde(rename = "LinkID")]
    link_id: LinkId,
    #[serde(rename = "AccessTime")]
    access_time: i64,
}

impl RecentlyAccessedRequest {
    pub fn new(items: &[RecentlyAccessedItem], now: DateTime<Utc>) -> Self {
        Self {
            recently_accessed_items: items
                .iter()
                .map(|item| RecentlyAccessedDto {
                    volume_id: item.node_uid.volume_id.clone(),
                    link_id: item.node_uid.link_id.clone(),
                    access_time: item.access_time.unwrap_or(now).timestamp(),
                })
                .collect(),
        }
    }
}

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
