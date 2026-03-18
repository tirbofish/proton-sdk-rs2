use crate::account::{AddressId, AddressKeyId};
use crate::api::ApiResponse;
use crate::api::links::{LinkDetailsDto, LinkType};
use crate::links::LinkId;
use crate::pgp::PgpArmoredSignature;
use crate::pgp::{PgpArmoredMessage, PgpArmoredPrivateKey};
use crate::share::ShareId;
use crate::volume::VolumeId;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use proton_sdk_rs2::auth::TokenCredential;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ShareMembershipId(String);

impl Default for ShareMembershipId {
    fn default() -> Self {
        Self(String::new())
    }
}

impl ShareMembershipId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn raw(&self) -> &str {
        &self.0
    }
}

#[async_trait]
pub trait SharesApiClient: Send + Sync {
    async fn get_my_files_share(&self) -> anyhow::Result<ShareResponseV2>;

    async fn get_share(&self, id: ShareId) -> anyhow::Result<ShareResponse>;
}

pub struct DefaultSharesApiClient {
    client: ClientWithMiddleware,
    base_url: reqwest::Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultSharesApiClient {
    pub fn new(
        client: ClientWithMiddleware,
        base_url: reqwest::Url,
        token_credential: Option<TokenCredential>,
    ) -> Self {
        Self {
            client,
            base_url,
            token_credential,
        }
    }
}

#[async_trait]
impl SharesApiClient for DefaultSharesApiClient {
    async fn get_my_files_share(&self) -> anyhow::Result<ShareResponseV2> {
        let url = self.base_url.join("v2/shares/my-files")?;
        let mut request = self.client.get(url);

        if let Some(credential) = &self.token_credential {
            let (access_token, _): (String, String) = credential.get_tokens().await?;
            request = request.header("Authorization", format!("Bearer {}", access_token));
            request = request.header("x-pm-uid", credential.session_id().raw());
        }

        let response = request.send().await?;
        let text = response.text().await?;
        tracing::debug!(body = %text, "get_my_files_share raw response");
        let response = serde_json::from_str::<ShareResponseV2>(&text)?;

        Ok(response)
    }

    async fn get_share(&self, id: ShareId) -> anyhow::Result<ShareResponse> {
        let url = self.base_url.join(&format!("shares/{}", id.raw()))?;
        let mut request = self.client.get(url);

        if let Some(credential) = &self.token_credential {
            let (access_token, _): (String, String) = credential.get_tokens().await?;
            request = request.header("Authorization", format!("Bearer {}", access_token));
            request = request.header("x-pm-uid", credential.session_id().raw());
        }

        let response = request.send().await?;
        Ok(response.json::<ShareResponse>().await?)
    }
}

#[derive(Debug, Deserialize)]
pub struct ContextShareResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    #[serde(rename = "ContextShareID")]
    pub context_share_id: ShareId,
}

impl ContextShareResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShareMembershipSummaryDto {
    #[serde(rename = "ShareID")]
    pub share_id: ShareId,

    #[serde(rename = "MembershipID")]
    pub membership_id: ShareMembershipId,

    #[serde(rename = "Permissions")]
    pub permissions: ShareMemberPermissions,
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ShareMemberPermissions: u32 {
        const NONE  = 0;
        const WRITE = 1 << 1;
        const READ  = 1 << 2;
        const ADMIN = 1 << 4;
    }
}

impl serde::Serialize for ShareMemberPermissions {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u32(self.bits())
    }
}

impl<'de> serde::Deserialize<'de> for ShareMemberPermissions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bits = u32::deserialize(deserializer)?;
        Ok(Self::from_bits_retain(bits))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum ShareType {
    Main = 1,
    Standard = 2,
    Device = 3,
    Photos = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum ShareState {
    Active = 1,
    Deleted = 2,
    Restored = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum ShareMembershipState {
    Active = 1,
    Locked = 3,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ShareDto {
    #[serde(rename = "ShareID")]
    pub id: ShareId,

    #[serde(rename = "CreatorEmail")]
    pub creator_email_address: String,

    pub key: PgpArmoredPrivateKey,
    pub passphrase: PgpArmoredMessage,
    pub passphrase_signature: PgpArmoredSignature,

    #[serde(rename = "AddressID")]
    pub address_id: AddressId,

    pub inviter_share_passphrase_key_packet_signature: Option<PgpArmoredSignature>,
    pub invitee_share_passphrase_session_key_signature: Option<PgpArmoredSignature>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ShareMembershipDto {
    #[serde(rename = "MemberID")]
    pub id: ShareMembershipId,

    #[serde(rename = "ShareID")]
    pub share_id: ShareId,

    #[serde(rename = "AddressID")]
    pub address_id: AddressId,

    #[serde(rename = "AddressKeyID")]
    pub address_key_id: AddressKeyId,

    #[serde(rename = "Inviter")]
    pub inviter_email_address: String,

    pub permissions: ShareMemberPermissions,

    #[serde(with = "crate::utils::serde::base64_bytes")]
    pub key_packet: Vec<u8>,

    pub key_packet_signature: Option<PgpArmoredSignature>,
    pub session_key_signature: Option<PgpArmoredSignature>,

    pub state: ShareMembershipState,

    #[serde(rename = "Unlockable")]
    pub can_be_unlocked: Option<bool>,

    #[serde(rename = "CreateTime")]
    #[serde(with = "crate::utils::serde::epoch_seconds")]
    pub creation_time: DateTime<Utc>,

    #[serde(rename = "ModifyTime")]
    #[serde(with = "crate::utils::serde::epoch_seconds")]
    pub modification_time: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ShareResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    #[serde(rename = "ShareID")]
    pub id: ShareId,

    #[serde(rename = "VolumeID")]
    pub volume_id: VolumeId,

    #[serde(rename = "Type")]
    pub r#type: ShareType,
    pub state: ShareState,

    #[serde(rename = "Creator")]
    pub creator_email_address: String,

    #[serde(rename = "Locked")]
    pub is_locked: bool,

    #[serde(rename = "CreateTime")]
    #[serde(default, with = "crate::utils::serde::epoch_seconds_opt")]
    pub creation_time: Option<DateTime<Utc>>,

    #[serde(rename = "ModifyTime")]
    #[serde(default, with = "crate::utils::serde::epoch_seconds_opt")]
    pub modification_time: Option<DateTime<Utc>>,

    #[serde(rename = "LinkID")]
    pub root_link_id: LinkId,

    #[serde(rename = "LinkType")]
    pub root_link_type: LinkType,

    pub key: PgpArmoredPrivateKey,
    pub passphrase: PgpArmoredMessage,
    pub passphrase_signature: PgpArmoredSignature,

    #[serde(rename = "AddressID")]
    pub address_id: AddressId,

    pub inviter_share_passphrase_key_packet_signature: Option<PgpArmoredSignature>,
    pub invitee_share_passphrase_session_key_signature: Option<PgpArmoredSignature>,

    pub memberships: Vec<ShareMembershipDto>,
}

impl ShareResponse {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ShareResponseV2 {
    #[serde(flatten)]
    pub base: ApiResponse,

    pub share: ShareDto,
    pub volume: ShareVolumeDto,

    #[serde(rename = "Link")]
    pub link_details: LinkDetailsDto,
}

impl ShareResponseV2 {
    pub fn is_success(&self) -> bool {
        self.base.is_success()
    }

    pub fn deconstruct(self) -> (ShareVolumeDto, ShareDto, LinkDetailsDto) {
        (self.volume, self.share, self.link_details)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ShareVolumeDto {
    #[serde(rename = "VolumeID")]
    pub id: VolumeId,

    pub used_space: i64,
}
