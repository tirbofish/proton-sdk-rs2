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

    async fn get_shared_by_me(
        &self,
        volume_id: VolumeId,
        anchor_id: Option<LinkId>,
    ) -> anyhow::Result<SharedByMeResponse>;

    async fn get_shared_with_me(
        &self,
        anchor_id: Option<LinkId>,
    ) -> anyhow::Result<SharedWithMeResponse>;

    async fn remove_member(
        &self,
        share_id: ShareId,
        member_id: ShareMembershipId,
    ) -> anyhow::Result<()>;

    async fn list_invitations(
        &self,
        share_target_types: &[ShareTargetType],
        anchor_id: Option<&str>,
    ) -> anyhow::Result<InvitationsListResponse>;

    async fn get_invitation(&self, invitation_id: &str) -> anyhow::Result<InvitationDetailsResponse>;

    async fn accept_invitation(
        &self,
        invitation_id: &str,
        session_key_signature: &str,
    ) -> anyhow::Result<()>;

    async fn reject_invitation(&self, invitation_id: &str) -> anyhow::Result<()>;

    async fn get_share_invitations(
        &self,
        share_id: ShareId,
    ) -> anyhow::Result<ShareInvitationsResponse>;

    async fn get_share_external_invitations(
        &self,
        share_id: ShareId,
    ) -> anyhow::Result<ShareExternalInvitationsResponse>;

    async fn get_share_members(&self, share_id: ShareId) -> anyhow::Result<ShareMembersResponse>;

    async fn create_standard_share(
        &self,
        volume_id: VolumeId,
        request: CreateShareRequest,
    ) -> anyhow::Result<CreateShareResponse>;

    async fn delete_share(&self, share_id: ShareId, force: bool) -> anyhow::Result<()>;

    async fn set_editors_can_share(&self, share_id: ShareId, value: bool) -> anyhow::Result<()>;

    async fn invite_proton_user(
        &self,
        share_id: ShareId,
        request: InviteProtonUserRequest,
    ) -> anyhow::Result<InviteProtonUserResponse>;

    async fn update_invitation(
        &self,
        share_id: ShareId,
        invitation_id: &str,
        permissions: u32,
    ) -> anyhow::Result<()>;

    async fn resend_invitation_email(
        &self,
        share_id: ShareId,
        invitation_id: &str,
    ) -> anyhow::Result<()>;

    async fn delete_invitation(&self, share_id: ShareId, invitation_id: &str) -> anyhow::Result<()>;

    async fn invite_external_user(
        &self,
        share_id: ShareId,
        request: InviteExternalUserRequest,
    ) -> anyhow::Result<InviteExternalUserResponse>;

    async fn update_external_invitation(
        &self,
        share_id: ShareId,
        invitation_id: &str,
        permissions: u32,
    ) -> anyhow::Result<()>;

    async fn resend_external_invitation_email(
        &self,
        share_id: ShareId,
        invitation_id: &str,
    ) -> anyhow::Result<()>;

    async fn delete_external_invitation(
        &self,
        share_id: ShareId,
        invitation_id: &str,
    ) -> anyhow::Result<()>;

    async fn update_member(
        &self,
        share_id: ShareId,
        member_id: ShareMembershipId,
        permissions: u32,
    ) -> anyhow::Result<()>;

    async fn get_share_urls(&self, share_id: ShareId) -> anyhow::Result<ShareUrlsResponse>;

    async fn create_share_url(
        &self,
        share_id: ShareId,
        request: CreateShareUrlRequest,
    ) -> anyhow::Result<CreateShareUrlResponse>;

    async fn delete_share_url(&self, share_id: ShareId, url_id: &str) -> anyhow::Result<()>;

    async fn get_public_link_info(&self, token: &str) -> anyhow::Result<PublicLinkInfoResponse>;

    async fn auth_public_link(
        &self,
        token: &str,
        request: PublicLinkAuthRequest,
    ) -> anyhow::Result<PublicLinkAuthResponse>;

    async fn list_bookmarks(&self) -> anyhow::Result<BookmarksResponse>;

    async fn create_bookmark(
        &self,
        token: &str,
        request: CreateBookmarkRequest,
    ) -> anyhow::Result<()>;

    async fn delete_bookmark(&self, token: &str) -> anyhow::Result<()>;

    async fn get_srp_modulus(&self) -> anyhow::Result<SrpModulusResponse>;
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

    async fn add_auth_headers(
        &self,
        mut builder: reqwest_middleware::RequestBuilder,
    ) -> anyhow::Result<reqwest_middleware::RequestBuilder> {
        if let Some(credential) = &self.token_credential {
            let (access_token, _) = credential.get_tokens().await?;
            builder = builder.header("Authorization", format!("Bearer {}", access_token));
            builder = builder.header("x-pm-uid", credential.session_id().raw());
        }
        Ok(builder)
    }

    async fn send_json<T: serde::de::DeserializeOwned>(
        &self,
        builder: reqwest_middleware::RequestBuilder,
    ) -> anyhow::Result<T> {
        let builder = self.add_auth_headers(builder).await?;
        let response = builder.send().await?;
        let text = response.text().await?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            anyhow::anyhow!("Failed to decode JSON: {}. Body: {}", e, text)
        })?;
        if let Some(code) = value.get("Code").and_then(|c| c.as_u64()) {
            if code != 1000 {
                let error = value
                    .get("Error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("Unknown error");
                anyhow::bail!("API error: code {}, message: {}", code, error);
            }
        }
        serde_json::from_value(value)
            .map_err(|e| anyhow::anyhow!("Failed to parse JSON: {}. Body: {}", e, text))
    }

    async fn send_ok(&self, builder: reqwest_middleware::RequestBuilder) -> anyhow::Result<()> {
        let builder = self.add_auth_headers(builder).await?;
        ApiResponse::from_response(builder.send().await?).await?.to_result()
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

        let res: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            anyhow::anyhow!("Failed to decode response body: {}. Body: {}", e, text)
        })?;

        if let Some(code) = res.get("Code").and_then(|c| c.as_u64()) {
            if code != 1000 {
                let error = res
                    .get("Error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("Unknown error");
                anyhow::bail!("API error: code {}, message: {}", code, error);
            }
        }

        let share_response: ShareResponseV2 = serde_json::from_value(res).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse my-files share response: {}. Body: {}",
                e,
                text
            )
        })?;
        Ok(share_response)
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
        let text = response.text().await?;
        tracing::debug!(body = %text, "get_share raw response");
        let res: ShareResponse = serde_json::from_str(&text).map_err(|e| {
            anyhow::anyhow!("Failed to decode share response: {}. Body: {}", e, text)
        })?;
        Ok(res)
    }

    async fn get_shared_by_me(
        &self,
        volume_id: VolumeId,
        anchor_id: Option<LinkId>,
    ) -> anyhow::Result<SharedByMeResponse> {
        let path = match &anchor_id {
            Some(anchor) => format!(
                "v2/volumes/{}/shares?AnchorID={}",
                volume_id.raw(),
                anchor.raw()
            ),
            None => format!("v2/volumes/{}/shares", volume_id.raw()),
        };
        let url = self.base_url.join(&path)?;
        let builder = self.add_auth_headers(self.client.get(url)).await?;
        let response = builder.send().await?;
        let text = response.text().await?;
        let res: SharedByMeResponse = serde_json::from_str(&text).map_err(|e| {
            anyhow::anyhow!(
                "Failed to decode shared-by-me response: {}. Body: {}",
                e,
                text
            )
        })?;
        res.base.to_result()?;
        Ok(res)
    }

    async fn get_shared_with_me(
        &self,
        anchor_id: Option<LinkId>,
    ) -> anyhow::Result<SharedWithMeResponse> {
        let path = match &anchor_id {
            Some(anchor) => format!("v2/sharedwithme?AnchorID={}", anchor.raw()),
            None => "v2/sharedwithme".to_string(),
        };
        let url = self.base_url.join(&path)?;
        let builder = self.add_auth_headers(self.client.get(url)).await?;
        let response = builder.send().await?;
        let text = response.text().await?;
        let res: SharedWithMeResponse = serde_json::from_str(&text).map_err(|e| {
            anyhow::anyhow!(
                "Failed to decode shared-with-me response: {}. Body: {}",
                e,
                text
            )
        })?;
        res.base.to_result()?;
        Ok(res)
    }

    async fn remove_member(
        &self,
        share_id: ShareId,
        member_id: ShareMembershipId,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "v2/shares/{}/members/{}",
            share_id.raw(),
            member_id.raw()
        ))?;
        self.send_ok(self.client.delete(url)).await
    }

    async fn list_invitations(
        &self,
        share_target_types: &[ShareTargetType],
        anchor_id: Option<&str>,
    ) -> anyhow::Result<InvitationsListResponse> {
        let mut query = share_target_types
            .iter()
            .map(|t| format!("ShareTargetTypes[]={}", *t as u32))
            .collect::<Vec<_>>()
            .join("&");
        if let Some(anchor) = anchor_id {
            if !query.is_empty() {
                query.push('&');
            }
            query.push_str(&format!("AnchorID={anchor}"));
        }
        let path = if query.is_empty() {
            "v2/shares/invitations".to_string()
        } else {
            format!("v2/shares/invitations?{query}")
        };
        let url = self.base_url.join(&path)?;
        self.send_json(self.client.get(url)).await
    }

    async fn get_invitation(&self, invitation_id: &str) -> anyhow::Result<InvitationDetailsResponse> {
        let url = self
            .base_url
            .join(&format!("v2/shares/invitations/{invitation_id}"))?;
        self.send_json(self.client.get(url)).await
    }

    async fn accept_invitation(
        &self,
        invitation_id: &str,
        session_key_signature: &str,
    ) -> anyhow::Result<()> {
        let url = self
            .base_url
            .join(&format!("v2/shares/invitations/{invitation_id}/accept"))?;
        self.send_ok(
            self.client
                .post(url)
                .json(&serde_json::json!({ "SessionKeySignature": session_key_signature })),
        )
        .await
    }

    async fn reject_invitation(&self, invitation_id: &str) -> anyhow::Result<()> {
        let url = self
            .base_url
            .join(&format!("v2/shares/invitations/{invitation_id}/reject"))?;
        self.send_ok(self.client.post(url)).await
    }

    async fn get_share_invitations(
        &self,
        share_id: ShareId,
    ) -> anyhow::Result<ShareInvitationsResponse> {
        let url = self
            .base_url
            .join(&format!("v2/shares/{}/invitations", share_id.raw()))?;
        self.send_json(self.client.get(url)).await
    }

    async fn get_share_external_invitations(
        &self,
        share_id: ShareId,
    ) -> anyhow::Result<ShareExternalInvitationsResponse> {
        let url = self
            .base_url
            .join(&format!("v2/shares/{}/external-invitations", share_id.raw()))?;
        self.send_json(self.client.get(url)).await
    }

    async fn get_share_members(&self, share_id: ShareId) -> anyhow::Result<ShareMembersResponse> {
        let url = self
            .base_url
            .join(&format!("v2/shares/{}/members", share_id.raw()))?;
        self.send_json(self.client.get(url)).await
    }

    async fn create_standard_share(
        &self,
        volume_id: VolumeId,
        request: CreateShareRequest,
    ) -> anyhow::Result<CreateShareResponse> {
        let url = self
            .base_url
            .join(&format!("volumes/{}/shares", volume_id.raw()))?;
        self.send_json(self.client.post(url).json(&request)).await
    }

    async fn delete_share(&self, share_id: ShareId, force: bool) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "shares/{}?Force={}",
            share_id.raw(),
            if force { 1 } else { 0 }
        ))?;
        self.send_ok(self.client.delete(url)).await
    }

    async fn set_editors_can_share(&self, share_id: ShareId, value: bool) -> anyhow::Result<()> {
        let url = self
            .base_url
            .join(&format!("shares/{}/editors-can-share", share_id.raw()))?;
        self.send_ok(self.client.put(url).json(&serde_json::json!({ "Value": value })))
            .await
    }

    async fn invite_proton_user(
        &self,
        share_id: ShareId,
        request: InviteProtonUserRequest,
    ) -> anyhow::Result<InviteProtonUserResponse> {
        let url = self
            .base_url
            .join(&format!("v2/shares/{}/invitations", share_id.raw()))?;
        self.send_json(self.client.post(url).json(&request)).await
    }

    async fn update_invitation(
        &self,
        share_id: ShareId,
        invitation_id: &str,
        permissions: u32,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "v2/shares/{}/invitations/{invitation_id}",
            share_id.raw()
        ))?;
        self.send_ok(
            self.client
                .put(url)
                .json(&serde_json::json!({ "Permissions": permissions })),
        )
        .await
    }

    async fn resend_invitation_email(
        &self,
        share_id: ShareId,
        invitation_id: &str,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "v2/shares/{}/invitations/{invitation_id}/sendemail",
            share_id.raw()
        ))?;
        self.send_ok(self.client.post(url)).await
    }

    async fn delete_invitation(&self, share_id: ShareId, invitation_id: &str) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "v2/shares/{}/invitations/{invitation_id}",
            share_id.raw()
        ))?;
        self.send_ok(self.client.delete(url)).await
    }

    async fn invite_external_user(
        &self,
        share_id: ShareId,
        request: InviteExternalUserRequest,
    ) -> anyhow::Result<InviteExternalUserResponse> {
        let url = self.base_url.join(&format!(
            "v2/shares/{}/external-invitations",
            share_id.raw()
        ))?;
        self.send_json(self.client.post(url).json(&request)).await
    }

    async fn update_external_invitation(
        &self,
        share_id: ShareId,
        invitation_id: &str,
        permissions: u32,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "v2/shares/{}/external-invitations/{invitation_id}",
            share_id.raw()
        ))?;
        self.send_ok(
            self.client
                .put(url)
                .json(&serde_json::json!({ "Permissions": permissions })),
        )
        .await
    }

    async fn resend_external_invitation_email(
        &self,
        share_id: ShareId,
        invitation_id: &str,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "v2/shares/{}/external-invitations/{invitation_id}/sendemail",
            share_id.raw()
        ))?;
        self.send_ok(self.client.post(url)).await
    }

    async fn delete_external_invitation(
        &self,
        share_id: ShareId,
        invitation_id: &str,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "v2/shares/{}/external-invitations/{invitation_id}",
            share_id.raw()
        ))?;
        self.send_ok(self.client.delete(url)).await
    }

    async fn update_member(
        &self,
        share_id: ShareId,
        member_id: ShareMembershipId,
        permissions: u32,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!(
            "v2/shares/{}/members/{}",
            share_id.raw(),
            member_id.raw()
        ))?;
        self.send_ok(
            self.client
                .put(url)
                .json(&serde_json::json!({ "Permissions": permissions })),
        )
        .await
    }

    async fn get_share_urls(&self, share_id: ShareId) -> anyhow::Result<ShareUrlsResponse> {
        let url = self
            .base_url
            .join(&format!("shares/{}/urls", share_id.raw()))?;
        self.send_json(self.client.get(url)).await
    }

    async fn create_share_url(
        &self,
        share_id: ShareId,
        request: CreateShareUrlRequest,
    ) -> anyhow::Result<CreateShareUrlResponse> {
        let url = self
            .base_url
            .join(&format!("shares/{}/urls", share_id.raw()))?;
        self.send_json(self.client.post(url).json(&request)).await
    }

    async fn delete_share_url(&self, share_id: ShareId, url_id: &str) -> anyhow::Result<()> {
        let url = self
            .base_url
            .join(&format!("shares/{}/urls/{url_id}", share_id.raw()))?;
        self.send_ok(self.client.delete(url)).await
    }

    async fn get_public_link_info(&self, token: &str) -> anyhow::Result<PublicLinkInfoResponse> {
        let url = self.base_url.join(&format!("urls/{token}/info"))?;
        self.send_json(self.client.get(url)).await
    }

    async fn auth_public_link(
        &self,
        token: &str,
        request: PublicLinkAuthRequest,
    ) -> anyhow::Result<PublicLinkAuthResponse> {
        let url = self.base_url.join(&format!("urls/{token}/auth"))?;
        self.send_json(self.client.post(url).json(&request)).await
    }

    async fn list_bookmarks(&self) -> anyhow::Result<BookmarksResponse> {
        let url = self.base_url.join("v2/shared-bookmarks")?;
        self.send_json(self.client.get(url)).await
    }

    async fn create_bookmark(
        &self,
        token: &str,
        request: CreateBookmarkRequest,
    ) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!("v2/urls/{token}/bookmark"))?;
        self.send_ok(self.client.post(url).json(&request)).await
    }

    async fn delete_bookmark(&self, token: &str) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!("v2/urls/{token}/bookmark"))?;
        self.send_ok(self.client.delete(url)).await
    }

    async fn get_srp_modulus(&self) -> anyhow::Result<SrpModulusResponse> {
        let url = self.base_url.join("/auth/v4/modulus")?;
        self.send_json(self.client.get(url)).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum ShareTargetType {
    Root = 0,
    Folder = 1,
    File = 2,
    Album = 3,
    Photo = 4,
    ProtonVendor = 5,
}

impl ShareTargetType {
    pub const DRIVE: &'static [ShareTargetType] = &[
        ShareTargetType::Folder,
        ShareTargetType::File,
        ShareTargetType::ProtonVendor,
    ];

    pub const PHOTOS: &'static [ShareTargetType] =
        &[ShareTargetType::Photo, ShareTargetType::Album];
}

#[derive(Debug, Clone, Deserialize)]
pub struct SharedByMeLinkDto {
    #[serde(rename = "ShareID")]
    pub share_id: ShareId,
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,
    #[serde(rename = "ContextShareID")]
    pub context_share_id: ShareId,
}

#[derive(Debug, Deserialize)]
pub struct SharedByMeResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "Links", default)]
    pub links: Vec<SharedByMeLinkDto>,
    #[serde(rename = "AnchorID")]
    pub anchor_id: Option<LinkId>,
    #[serde(rename = "More", default)]
    pub more: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SharedWithMeLinkDto {
    #[serde(rename = "VolumeID")]
    pub volume_id: VolumeId,
    #[serde(rename = "ShareID")]
    pub share_id: ShareId,
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,
    #[serde(rename = "ShareTargetType")]
    pub share_target_type: ShareTargetType,
}

#[derive(Debug, Deserialize)]
pub struct SharedWithMeResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "Links", default)]
    pub links: Vec<SharedWithMeLinkDto>,
    #[serde(rename = "AnchorID")]
    pub anchor_id: Option<LinkId>,
    #[serde(rename = "More", default)]
    pub more: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRole {
    Viewer,
    Editor,
    Admin,
}

impl Default for MemberRole {
    fn default() -> Self {
        Self::Viewer
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

impl ShareMemberPermissions {
    pub fn viewer() -> Self {
        Self::READ
    }

    pub fn editor() -> Self {
        Self::READ | Self::WRITE
    }

    pub fn admin() -> Self {
        Self::READ | Self::WRITE | Self::ADMIN
    }

    pub fn from_role(role: MemberRole) -> Self {
        match role {
            MemberRole::Viewer => Self::viewer(),
            MemberRole::Editor => Self::editor(),
            MemberRole::Admin => Self::admin(),
        }
    }

    pub fn to_role(self) -> MemberRole {
        if self.contains(Self::ADMIN) {
            MemberRole::Admin
        } else if self.contains(Self::WRITE) {
            MemberRole::Editor
        } else {
            MemberRole::Viewer
        }
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

    #[serde(default)]
    pub memberships: Vec<ShareMembershipDto>,

    #[serde(default, rename = "EditorsCanShare")]
    pub editors_can_share: bool,
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

#[derive(Debug, Clone, Deserialize)]
pub struct InvitationsListResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "Invitations", default)]
    pub invitations: Vec<PendingInvitationDto>,
    #[serde(rename = "AnchorID")]
    pub anchor_id: Option<String>,
    #[serde(rename = "More", default)]
    pub more: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PendingInvitationDto {
    #[serde(rename = "ShareID")]
    pub share_id: ShareId,
    #[serde(rename = "InvitationID")]
    pub invitation_id: String,
    #[serde(rename = "ShareTargetType", default)]
    pub share_target_type: Option<ShareTargetType>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InvitationDetailsResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "Invitation")]
    pub invitation: InvitationDto,
    #[serde(rename = "Share")]
    pub share: InvitationShareDto,
    #[serde(rename = "Link")]
    pub link: InvitationLinkDto,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InvitationDto {
    #[serde(rename = "InvitationID")]
    pub invitation_id: String,
    #[serde(rename = "InviterEmail")]
    pub inviter_email: String,
    #[serde(rename = "InviteeEmail")]
    pub invitee_email: String,
    #[serde(rename = "Permissions")]
    pub permissions: ShareMemberPermissions,
    #[serde(rename = "KeyPacket")]
    pub key_packet: String,
    #[serde(rename = "KeyPacketSignature")]
    pub key_packet_signature: String,
    #[serde(rename = "CreateTime")]
    #[serde(default, with = "crate::utils::serde::epoch_seconds_opt")]
    pub create_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InvitationShareDto {
    #[serde(rename = "ShareKey")]
    pub share_key: PgpArmoredPrivateKey,
    #[serde(rename = "Passphrase")]
    pub passphrase: PgpArmoredMessage,
    #[serde(rename = "CreatorEmail")]
    pub creator_email: String,
    #[serde(rename = "VolumeID")]
    pub volume_id: VolumeId,
    #[serde(rename = "ShareTargetType", default)]
    pub share_target_type: Option<ShareTargetType>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InvitationLinkDto {
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,
    #[serde(rename = "Type", default)]
    pub r#type: Option<LinkType>,
    #[serde(rename = "MIMEType", default)]
    pub mime_type: Option<String>,
    #[serde(rename = "Name", default)]
    pub name: Option<PgpArmoredMessage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShareInvitationsResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "Invitations", default)]
    pub invitations: Vec<ShareInvitationDto>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShareInvitationDto {
    #[serde(rename = "InvitationID")]
    pub invitation_id: String,
    #[serde(rename = "InviterEmail")]
    pub inviter_email: String,
    #[serde(rename = "InviteeEmail")]
    pub invitee_email: String,
    #[serde(rename = "Permissions")]
    pub permissions: ShareMemberPermissions,
    #[serde(rename = "KeyPacket")]
    pub key_packet: String,
    #[serde(rename = "KeyPacketSignature")]
    pub key_packet_signature: String,
    #[serde(rename = "CreateTime")]
    #[serde(default, with = "crate::utils::serde::epoch_seconds_opt")]
    pub create_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShareExternalInvitationsResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "ExternalInvitations", default)]
    pub external_invitations: Vec<ShareExternalInvitationDto>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShareExternalInvitationDto {
    #[serde(rename = "ExternalInvitationID")]
    pub invitation_id: String,
    #[serde(rename = "InviterEmail")]
    pub inviter_email: String,
    #[serde(rename = "InviteeEmail")]
    pub invitee_email: String,
    #[serde(rename = "Permissions")]
    pub permissions: ShareMemberPermissions,
    #[serde(rename = "ExternalInvitationSignature")]
    pub signature: String,
    #[serde(rename = "State", default)]
    pub state: u32,
    #[serde(rename = "CreateTime")]
    #[serde(default, with = "crate::utils::serde::epoch_seconds_opt")]
    pub create_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShareMembersResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "Members", default)]
    pub members: Vec<ShareMemberDto>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShareMemberDto {
    #[serde(rename = "MemberID")]
    pub member_id: ShareMembershipId,
    #[serde(rename = "InviterEmail")]
    pub inviter_email: String,
    #[serde(rename = "Email")]
    pub email: String,
    #[serde(rename = "Permissions")]
    pub permissions: ShareMemberPermissions,
    #[serde(rename = "KeyPacket")]
    pub key_packet: String,
    #[serde(rename = "KeyPacketSignature", default)]
    pub key_packet_signature: Option<String>,
    #[serde(rename = "CreateTime")]
    #[serde(default, with = "crate::utils::serde::epoch_seconds_opt")]
    pub create_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateShareRequest {
    #[serde(rename = "RootLinkID")]
    pub root_link_id: String,
    #[serde(rename = "AddressID")]
    pub address_id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "ShareKey")]
    pub share_key: String,
    #[serde(rename = "SharePassphrase")]
    pub share_passphrase: String,
    #[serde(rename = "SharePassphraseSignature")]
    pub share_passphrase_signature: String,
    #[serde(rename = "PassphraseKeyPacket")]
    pub passphrase_key_packet: String,
    #[serde(rename = "NameKeyPacket")]
    pub name_key_packet: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateShareResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "Share")]
    pub share: CreatedShareDto,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatedShareDto {
    #[serde(rename = "ID")]
    pub id: ShareId,
    #[serde(rename = "EditorsCanShare", default)]
    pub editors_can_share: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InviteProtonUserRequest {
    #[serde(rename = "Invitation")]
    pub invitation: InviteProtonUserBody,
    #[serde(rename = "EmailDetails")]
    pub email_details: EmailDetailsBody,
}

#[derive(Debug, Clone, Serialize)]
pub struct InviteProtonUserBody {
    #[serde(rename = "InviterEmail")]
    pub inviter_email: String,
    #[serde(rename = "InviteeEmail")]
    pub invitee_email: String,
    #[serde(rename = "Permissions")]
    pub permissions: u32,
    #[serde(rename = "KeyPacket")]
    pub key_packet: String,
    #[serde(rename = "KeyPacketSignature")]
    pub key_packet_signature: String,
    #[serde(rename = "ExternalInvitationID", skip_serializing_if = "Option::is_none")]
    pub external_invitation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct EmailDetailsBody {
    #[serde(rename = "Message", skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(rename = "ItemName", skip_serializing_if = "Option::is_none")]
    pub item_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InviteProtonUserResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "Invitation")]
    pub invitation: ShareInvitationDto,
}

#[derive(Debug, Clone, Serialize)]
pub struct InviteExternalUserRequest {
    #[serde(rename = "ExternalInvitation")]
    pub invitation: InviteExternalUserBody,
    #[serde(rename = "EmailDetails")]
    pub email_details: EmailDetailsBody,
}

#[derive(Debug, Clone, Serialize)]
pub struct InviteExternalUserBody {
    #[serde(rename = "InviterAddressID")]
    pub inviter_address_id: String,
    #[serde(rename = "InviteeEmail")]
    pub invitee_email: String,
    #[serde(rename = "Permissions")]
    pub permissions: u32,
    #[serde(rename = "ExternalInvitationSignature")]
    pub signature: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InviteExternalUserResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "ExternalInvitation")]
    pub invitation: ShareExternalInvitationDto,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShareUrlsResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "ShareURLs", default)]
    pub share_urls: Vec<ShareUrlDto>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShareUrlDto {
    #[serde(rename = "ShareID")]
    pub share_id: ShareId,
    #[serde(rename = "ShareURLID")]
    pub share_url_id: String,
    #[serde(rename = "CreateTime")]
    #[serde(default, with = "crate::utils::serde::epoch_seconds_opt")]
    pub create_time: Option<DateTime<Utc>>,
    #[serde(rename = "ExpirationTime")]
    #[serde(default)]
    pub expiration_time: Option<i64>,
    #[serde(rename = "Permissions")]
    pub permissions: ShareMemberPermissions,
    #[serde(rename = "Flags", default)]
    pub flags: u32,
    #[serde(rename = "CreatorEmail")]
    pub creator_email: String,
    #[serde(rename = "PublicUrl")]
    pub public_url: String,
    #[serde(rename = "NumAccesses", default)]
    pub num_accesses: u32,
    #[serde(rename = "Password")]
    pub password: PgpArmoredMessage,
    #[serde(rename = "UrlPasswordSalt", default)]
    pub url_password_salt: String,
    #[serde(rename = "SharePassphraseKeyPacket", default)]
    pub share_passphrase_key_packet: String,
    #[serde(rename = "SharePasswordSalt", default)]
    pub share_password_salt: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateShareUrlRequest {
    #[serde(rename = "CreatorEmail")]
    pub creator_email: String,
    #[serde(rename = "Permissions")]
    pub permissions: u32,
    #[serde(rename = "Flags")]
    pub flags: u32,
    #[serde(rename = "ExpirationTime")]
    pub expiration_time: Option<i64>,
    #[serde(rename = "SharePasswordSalt")]
    pub share_password_salt: String,
    #[serde(rename = "SharePassphraseKeyPacket")]
    pub share_passphrase_key_packet: String,
    #[serde(rename = "Password")]
    pub password: String,
    #[serde(rename = "UrlPasswordSalt")]
    pub url_password_salt: String,
    #[serde(rename = "SRPVerifier")]
    pub srp_verifier: String,
    #[serde(rename = "SRPModulusID")]
    pub srp_modulus_id: String,
    #[serde(rename = "MaxAccesses")]
    pub max_accesses: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateShareUrlResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "ShareURL")]
    pub share_url: CreatedShareUrlDto,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatedShareUrlDto {
    #[serde(rename = "ShareURLID")]
    pub share_url_id: String,
    #[serde(rename = "PublicUrl")]
    pub public_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublicLinkInfoResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "Version")]
    pub version: u8,
    #[serde(rename = "Modulus")]
    pub modulus: String,
    #[serde(rename = "ServerEphemeral")]
    pub server_ephemeral: String,
    #[serde(rename = "UrlPasswordSalt")]
    pub url_password_salt: String,
    #[serde(rename = "SRPSession")]
    pub srp_session: String,
    #[serde(rename = "Flags", default)]
    pub flags: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublicLinkAuthRequest {
    #[serde(rename = "ClientProof")]
    pub client_proof: String,
    #[serde(rename = "ClientEphemeral")]
    pub client_ephemeral: String,
    #[serde(rename = "SRPSession")]
    pub srp_session: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublicLinkAuthResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "ServerProof")]
    pub server_proof: String,
    #[serde(rename = "UID")]
    pub session_uid: String,
    #[serde(rename = "AccessToken")]
    pub access_token: String,
    #[serde(rename = "Share")]
    pub share: PublicLinkShareDto,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublicLinkShareDto {
    #[serde(rename = "SharePasswordSalt")]
    pub share_password_salt: String,
    #[serde(rename = "ShareKey")]
    pub share_key: PgpArmoredPrivateKey,
    #[serde(rename = "SharePassphrase")]
    pub share_passphrase: PgpArmoredMessage,
    #[serde(rename = "PublicPermissions", default)]
    pub public_permissions: ShareMemberPermissions,
    #[serde(rename = "VolumeID")]
    pub volume_id: VolumeId,
    #[serde(rename = "LinkID")]
    pub link_id: LinkId,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BookmarksResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "Bookmarks", default)]
    pub bookmarks: Vec<BookmarkDto>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BookmarkDto {
    #[serde(rename = "CreateTime")]
    #[serde(default, with = "crate::utils::serde::epoch_seconds_opt")]
    pub create_time: Option<DateTime<Utc>>,
    #[serde(rename = "EncryptedUrlPassword", default)]
    pub encrypted_url_password: Option<PgpArmoredMessage>,
    #[serde(rename = "Token")]
    pub token: BookmarkTokenDto,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BookmarkTokenDto {
    #[serde(rename = "Token")]
    pub token: String,
    #[serde(rename = "ShareKey")]
    pub share_key: PgpArmoredPrivateKey,
    #[serde(rename = "SharePassphrase")]
    pub share_passphrase: PgpArmoredMessage,
    #[serde(rename = "SharePasswordSalt", default)]
    pub share_password_salt: String,
    #[serde(rename = "LinkType", default)]
    pub link_type: Option<LinkType>,
    #[serde(rename = "MIMEType", default)]
    pub mime_type: Option<String>,
    #[serde(rename = "Name", default)]
    pub name: Option<PgpArmoredMessage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateBookmarkRequest {
    #[serde(rename = "BookmarkShareURL")]
    pub bookmark_share_url: CreateBookmarkBody,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateBookmarkBody {
    #[serde(rename = "EncryptedUrlPassword")]
    pub encrypted_url_password: String,
    #[serde(rename = "AddressID")]
    pub address_id: String,
    #[serde(rename = "AddressKeyID")]
    pub address_key_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SrpModulusResponse {
    #[serde(flatten)]
    pub base: ApiResponse,
    #[serde(rename = "Modulus")]
    pub modulus: String,
    #[serde(rename = "ModulusID")]
    pub modulus_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_with_me_response_parses() {
        let json = r#"{
            "Code": 1000,
            "Links": [{
                "VolumeID": "vol",
                "ShareID": "share",
                "LinkID": "link",
                "ShareTargetType": 2
            }],
            "AnchorID": "next",
            "More": true
        }"#;
        let res: SharedWithMeResponse = serde_json::from_str(json).unwrap();
        assert!(res.base.is_success());
        assert_eq!(res.links.len(), 1);
        assert_eq!(res.links[0].share_target_type, ShareTargetType::File);
        assert!(res.more);
    }
}
