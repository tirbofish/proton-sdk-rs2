use crate::account::AddressId;
use crate::api::share::{
    CreateBookmarkBody, CreateBookmarkRequest, CreateShareRequest, CreateShareUrlRequest,
    EmailDetailsBody, InviteExternalUserBody, InviteExternalUserRequest, InviteProtonUserBody,
    InviteProtonUserRequest, PublicLinkAuthRequest, ShareMemberPermissions, ShareTargetType,
};
use crate::client::ProtonDriveClient;
use crate::crypto::CryptoGenerator;
use crate::error::ProtonDriveError;
use crate::node::crypto::NodeCrypto;
use crate::node::{NodeAndSecrets, NodeSecrets, NodeUid};
use crate::pgp::{PgpPrivateKey, PgpSessionKey};
use crate::share::ShareId;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use proton_rpgp::{
    AsPublicKeyRef, DataEncoding, Encryptor, PublicKey, SignatureContext, Signer,
};
use proton_srp::{SRPAuth, SRPVerifierB64, SrpHashVersion};
use rand::RngExt;

pub use crate::api::share::MemberRole;

const SIGNING_INVITER: &str = "drive.share-member.inviter";
const SIGNING_MEMBER: &str = "drive.share-member.member";
const SIGNING_EXTERNAL: &str = "drive.share-member.external-invitation";
const PUBLIC_LINK_PASSWORD_LEN: usize = 12;
const GENERATED_PASSWORD_CHARSET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

#[derive(Debug, Clone)]
pub struct ShareMember {
    pub uid: String,
    pub invitee_email: String,
    pub added_by_email: String,
    pub role: MemberRole,
    pub invitation_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct ProtonInvitation {
    pub uid: String,
    pub invitee_email: String,
    pub added_by_email: String,
    pub role: MemberRole,
    pub invitation_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonProtonInvitationState {
    Pending,
    UserRegistered,
}

#[derive(Debug, Clone)]
pub struct NonProtonInvitation {
    pub uid: String,
    pub invitee_email: String,
    pub added_by_email: String,
    pub role: MemberRole,
    pub state: NonProtonInvitationState,
    pub invitation_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct ProtonInvitationWithNode {
    pub invitation: ProtonInvitation,
    pub node_uid: NodeUid,
    pub node_name: Option<String>,
    pub media_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UrlAccess {
    pub uid: String,
    pub url: String,
    pub role: MemberRole,
    pub creation_time: Option<DateTime<Utc>>,
    pub expiration_time: Option<DateTime<Utc>>,
    pub custom_password: Option<String>,
    pub number_of_initialized_downloads: u32,
}

#[derive(Debug, Clone)]
pub struct ShareResult {
    pub proton_invitations: Vec<ProtonInvitation>,
    pub non_proton_invitations: Vec<NonProtonInvitation>,
    pub members: Vec<ShareMember>,
    pub url_access: Option<UrlAccess>,
    pub editors_can_share: bool,
}

#[derive(Debug, Clone)]
pub struct ShareUser {
    pub email: String,
    pub role: MemberRole,
}

#[derive(Debug, Clone, Default)]
pub struct ShareUrlSettings {
    pub role: MemberRole,
    pub custom_password: Option<String>,
    pub expiration: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
pub struct ShareNodeSettings {
    pub users: Vec<ShareUser>,
    pub url_access: Option<ShareUrlSettings>,
    pub email_message: Option<String>,
    pub email_node_name: Option<String>,
    pub editors_can_share: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct UnshareNodeSettings {
    pub users: Vec<String>,
    pub remove_url_access: bool,
}

#[derive(Debug, Clone)]
pub struct Bookmark {
    pub uid: String,
    pub url: String,
    pub creation_time: Option<DateTime<Utc>>,
    pub custom_password: Option<String>,
    pub node_name: Option<String>,
    pub media_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PublicLinkInfo {
    pub token: String,
    pub password: String,
    pub is_custom_password_protected: bool,
}

#[derive(Debug, Clone)]
pub struct PublicLinkSession {
    pub token: String,
    pub session_uid: String,
    pub access_token: String,
    pub root_uid: NodeUid,
    pub share_key: PgpPrivateKey,
    pub public_role: MemberRole,
}

pub struct PublicLinkClient {
    session: PublicLinkSession,
}

impl PublicLinkClient {
    pub fn session(&self) -> &PublicLinkSession {
        &self.session
    }

    pub fn root_uid(&self) -> &NodeUid {
        &self.session.root_uid
    }

    pub fn share_key(&self) -> &PgpPrivateKey {
        &self.session.share_key
    }
}

pub fn invitation_uid(share_id: &ShareId, invitation_id: &str) -> String {
    format!("{}~{}", share_id.raw(), invitation_id)
}

pub fn split_sharing_uid(uid: &str) -> anyhow::Result<(&str, &str)> {
    uid.split_once('~')
        .ok_or_else(|| anyhow::anyhow!("invalid sharing uid: {uid}"))
}

pub fn parse_public_link_url(url: &str) -> anyhow::Result<(String, String)> {
    let parsed = reqwest::Url::parse(url).map_err(|_| ProtonDriveError::Validation("Invalid URL".into()))?;
    let token = parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ProtonDriveError::Validation("Invalid URL".into()))?;
    let password = parsed.fragment().unwrap_or("");
    if password.is_empty() {
        anyhow::bail!(ProtonDriveError::Validation("Invalid URL".into()));
    }
    Ok((token.to_string(), password.to_string()))
}

pub struct SharingOperations;

impl SharingOperations {
    pub async fn iterate_invitations(
        client: &ProtonDriveClient,
        share_target_types: &[ShareTargetType],
    ) -> anyhow::Result<Vec<ProtonInvitationWithNode>> {
        let mut out = Vec::new();
        let mut anchor = None;
        loop {
            let response = client
                .api()
                .shares()
                .list_invitations(share_target_types, anchor.as_deref())
                .await?;
            for invitation in &response.invitations {
                if let Some(ty) = invitation.share_target_type {
                    if !share_target_types.contains(&ty) {
                        continue;
                    }
                }
                let uid = invitation_uid(&invitation.share_id, &invitation.invitation_id);
                out.push(Self::get_invitation(client, &uid).await?);
            }
            if !response.more || response.anchor_id.is_none() {
                break;
            }
            anchor = response.anchor_id;
        }
        Ok(out)
    }

    pub async fn get_invitation(
        client: &ProtonDriveClient,
        invitation_uid: &str,
    ) -> anyhow::Result<ProtonInvitationWithNode> {
        let (_, invitation_id) = split_sharing_uid(invitation_uid)?;
        let details = client.api().shares().get_invitation(invitation_id).await?;
        let node_uid = NodeUid::new(details.share.volume_id, details.link.link_id);
        let mut node_name = None;
        if let Some(encrypted_name) = &details.link.name {
            let address_keys = client
                .account()
                .get_address_private_keys(&AddressId::new(
                    client.account().get_default_address().await?.address_id,
                ))
                .await?;
            let pgp_keys: Vec<PgpPrivateKey> = address_keys.into_iter().map(PgpPrivateKey).collect();
            let claim = crate::node::authorship::AuthorshipClaim {
                keys: vec![],
                author: crate::author::Author::ANONYMOUS,
                key_retrieval_error_message: None,
            };
            if let Ok((passphrase, _, _)) = NodeCrypto::decrypt_message(
                &details.share.passphrase,
                None,
                &pgp_keys,
                &claim,
            ) {
                if let Ok(share_key) =
                    NodeCrypto::unlock_key_with_passphrase(&details.share.share_key, &passphrase)
                {
                    if let Ok((name, _, _)) =
                        NodeCrypto::decrypt_message(encrypted_name, None, [&share_key], &claim)
                    {
                        node_name = String::from_utf8(name).ok();
                    }
                }
            }
        }
        Ok(ProtonInvitationWithNode {
            invitation: ProtonInvitation {
                uid: invitation_uid.to_string(),
                invitee_email: details.invitation.invitee_email,
                added_by_email: details.invitation.inviter_email,
                role: details.invitation.permissions.to_role(),
                invitation_time: details.invitation.create_time,
            },
            node_uid,
            node_name,
            media_type: details.link.mime_type,
        })
    }

    pub async fn accept_invitation(
        client: &ProtonDriveClient,
        invitation_uid: &str,
    ) -> anyhow::Result<()> {
        let (_, invitation_id) = split_sharing_uid(invitation_uid)?;
        let details = client.api().shares().get_invitation(invitation_id).await?;
        let address = client.account().get_default_address().await?;
        let keys = client
            .account()
            .get_address_private_keys(&AddressId::new(address.address_id.clone()))
            .await?;
        let key_packet = STANDARD.decode(details.invitation.key_packet.as_bytes())?;
        let mut session_key = None;
        for key in &keys {
            if let Ok(sk) = PgpPrivateKey(key.clone()).decrypt_session_key(&key_packet) {
                session_key = Some(sk);
                break;
            }
        }
        let session_key =
            session_key.ok_or_else(|| anyhow::anyhow!("could not decrypt invitation key packet"))?;
        let signing_key = keys
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no address key to sign invitation accept"))?;
        let context = SignatureContext::new(SIGNING_MEMBER.to_string(), true);
        let signature = Signer::default()
            .with_signing_key(&signing_key)
            .with_signature_context(context)
            .sign_detached(&session_key.key, DataEncoding::Unarmored)?;
        client
            .api()
            .shares()
            .accept_invitation(invitation_id, &STANDARD.encode(signature))
            .await
    }

    pub async fn reject_invitation(
        client: &ProtonDriveClient,
        invitation_uid: &str,
    ) -> anyhow::Result<()> {
        let (_, invitation_id) = split_sharing_uid(invitation_uid)?;
        client.api().shares().reject_invitation(invitation_id).await
    }

    pub async fn resend_invitation_email(
        client: &ProtonDriveClient,
        invitation_uid: &str,
    ) -> anyhow::Result<()> {
        let (share_id, invitation_id) = split_sharing_uid(invitation_uid)?;
        let share_id = ShareId::new(share_id.to_string());
        if client
            .api()
            .shares()
            .resend_invitation_email(share_id.clone(), invitation_id)
            .await
            .is_ok()
        {
            return Ok(());
        }
        client
            .api()
            .shares()
            .resend_external_invitation_email(share_id, invitation_id)
            .await
    }

    pub async fn convert_non_proton_invitation(
        client: &ProtonDriveClient,
        node_uid: NodeUid,
        invitation_uid: &str,
    ) -> anyhow::Result<ProtonInvitation> {
        let (_, external_id) = split_sharing_uid(invitation_uid)?;
        let sharing = Self::get_sharing_info(client, node_uid.clone())
            .await?
            .ok_or_else(|| ProtonDriveError::Validation("This item is no longer shared".into()))?;
        let external = sharing
            .non_proton_invitations
            .iter()
            .find(|i| i.uid == invitation_uid)
            .cloned()
            .ok_or_else(|| ProtonDriveError::Validation("Invitation not found".into()))?;
        let ctx = Self::load_share_context(client, node_uid).await?;
        Self::invite_proton(
            client,
            &ctx,
            &external.invitee_email,
            external.role,
            Some(external_id),
            None,
            None,
        )
        .await
    }

    pub async fn get_sharing_info(
        client: &ProtonDriveClient,
        node_uid: NodeUid,
    ) -> anyhow::Result<Option<ShareResult>> {
        let Some(share_id) = Self::node_share_id(client, node_uid.clone()).await? else {
            return Ok(None);
        };
        let share = client.api().shares().get_share(share_id.clone()).await?;
        let proton_invitations = client
            .api()
            .shares()
            .get_share_invitations(share_id.clone())
            .await?
            .invitations
            .into_iter()
            .map(|i| ProtonInvitation {
                uid: invitation_uid(&share_id, &i.invitation_id),
                invitee_email: i.invitee_email,
                added_by_email: i.inviter_email,
                role: i.permissions.to_role(),
                invitation_time: i.create_time,
            })
            .collect();
        let non_proton_invitations = client
            .api()
            .shares()
            .get_share_external_invitations(share_id.clone())
            .await?
            .external_invitations
            .into_iter()
            .map(|i| NonProtonInvitation {
                uid: invitation_uid(&share_id, &i.invitation_id),
                invitee_email: i.invitee_email,
                added_by_email: i.inviter_email,
                role: i.permissions.to_role(),
                state: if i.state == 1 {
                    NonProtonInvitationState::Pending
                } else {
                    NonProtonInvitationState::UserRegistered
                },
                invitation_time: i.create_time,
            })
            .collect();
        let members = client
            .api()
            .shares()
            .get_share_members(share_id.clone())
            .await?
            .members
            .into_iter()
            .map(|m| ShareMember {
                uid: invitation_uid(&share_id, m.member_id.raw()),
                invitee_email: m.email,
                added_by_email: m.inviter_email,
                role: m.permissions.to_role(),
                invitation_time: m.create_time,
            })
            .collect();
        let url_access = Self::decrypt_url_access(client, share_id).await?;
        Ok(Some(ShareResult {
            proton_invitations,
            non_proton_invitations,
            members,
            url_access,
            editors_can_share: share.editors_can_share,
        }))
    }

    pub async fn share_node(
        client: &ProtonDriveClient,
        node_uid: NodeUid,
        settings: ShareNodeSettings,
    ) -> anyhow::Result<ShareResult> {
        if let Some(url) = &settings.url_access {
            if let Some(exp) = url.expiration {
                if exp < Utc::now() {
                    anyhow::bail!(ProtonDriveError::Validation(
                        "Expiration date cannot be in the past".into()
                    ));
                }
            }
        }

        let mut proton_users = Vec::new();
        let mut external_users = Vec::new();
        for user in &settings.users {
            if client
                .account()
                .get_address_public_keys(&user.email)
                .await?
                .is_empty()
            {
                external_users.push(user.clone());
            } else {
                proton_users.push(user.clone());
            }
        }

        let ctx = match Self::load_share_context(client, node_uid.clone()).await {
            Ok(ctx) => ctx,
            Err(_) => Self::create_share(client, node_uid.clone()).await?,
        };

        if let Some(value) = settings.editors_can_share {
            client
                .api()
                .shares()
                .set_editors_can_share(ctx.share_id.clone(), value)
                .await?;
        }

        let current = Self::get_sharing_info(client, node_uid.clone())
            .await?
            .unwrap_or(ShareResult {
                proton_invitations: vec![],
                non_proton_invitations: vec![],
                members: vec![],
                url_access: None,
                editors_can_share: ctx.editors_can_share,
            });

        for user in proton_users {
            let already = current
                .proton_invitations
                .iter()
                .any(|i| i.invitee_email.eq_ignore_ascii_case(&user.email))
                || current
                    .members
                    .iter()
                    .any(|m| m.invitee_email.eq_ignore_ascii_case(&user.email));
            if already {
                continue;
            }
            Self::invite_proton(
                client,
                &ctx,
                &user.email,
                user.role,
                None,
                settings.email_message.clone(),
                settings.email_node_name.clone(),
            )
            .await?;
        }
        for user in external_users {
            let already = current
                .non_proton_invitations
                .iter()
                .any(|i| i.invitee_email.eq_ignore_ascii_case(&user.email));
            if already {
                continue;
            }
            Self::invite_external(
                client,
                &ctx,
                &user.email,
                user.role,
                settings.email_message.clone(),
                settings.email_node_name.clone(),
            )
            .await?;
        }
        if let Some(url_settings) = settings.url_access {
            if current.url_access.is_none() {
                Self::create_public_link_on_share(client, &ctx, url_settings).await?;
            }
        }

        Ok(Self::get_sharing_info(client, node_uid)
            .await?
            .ok_or_else(|| anyhow::anyhow!("sharing info missing after share_node"))?)
    }

    pub async fn unshare_node(
        client: &ProtonDriveClient,
        node_uid: NodeUid,
        settings: UnshareNodeSettings,
    ) -> anyhow::Result<Option<ShareResult>> {
        let Some(current) = Self::get_sharing_info(client, node_uid.clone()).await? else {
            return Ok(None);
        };
        let share_id = Self::node_share_id(client, node_uid.clone())
            .await?
            .ok_or_else(|| anyhow::anyhow!("share missing"))?;

        if settings.remove_url_access {
            if let Some(url) = &current.url_access {
                let (_, url_id) = split_sharing_uid(&url.uid)?;
                client
                    .api()
                    .shares()
                    .delete_share_url(share_id.clone(), url_id)
                    .await?;
            }
        }

        for email in &settings.users {
            if let Some(member) = current
                .members
                .iter()
                .find(|m| m.invitee_email.eq_ignore_ascii_case(email))
            {
                let (_, member_id) = split_sharing_uid(&member.uid)?;
                client
                    .api()
                    .shares()
                    .remove_member(
                        share_id.clone(),
                        crate::api::share::ShareMembershipId::new(member_id.to_string()),
                    )
                    .await?;
            }
            if let Some(inv) = current
                .proton_invitations
                .iter()
                .find(|i| i.invitee_email.eq_ignore_ascii_case(email))
            {
                let (_, invitation_id) = split_sharing_uid(&inv.uid)?;
                client
                    .api()
                    .shares()
                    .delete_invitation(share_id.clone(), invitation_id)
                    .await?;
            }
            if let Some(inv) = current
                .non_proton_invitations
                .iter()
                .find(|i| i.invitee_email.eq_ignore_ascii_case(email))
            {
                let (_, invitation_id) = split_sharing_uid(&inv.uid)?;
                client
                    .api()
                    .shares()
                    .delete_external_invitation(share_id.clone(), invitation_id)
                    .await?;
            }
        }

        let remaining = Self::get_sharing_info(client, node_uid.clone()).await?;
        if let Some(info) = &remaining {
            if info.members.is_empty()
                && info.proton_invitations.is_empty()
                && info.non_proton_invitations.is_empty()
                && info.url_access.is_none()
            {
                client.api().shares().delete_share(share_id, true).await?;
                return Ok(None);
            }
        }
        Ok(remaining)
    }

    pub async fn set_editors_can_share(
        client: &ProtonDriveClient,
        node_uid: NodeUid,
        value: bool,
    ) -> anyhow::Result<()> {
        let share_id = Self::node_share_id(client, node_uid)
            .await?
            .ok_or_else(|| ProtonDriveError::Validation("Node is not shared".into()))?;
        client
            .api()
            .shares()
            .set_editors_can_share(share_id, value)
            .await
    }

    pub async fn create_public_link(
        client: &ProtonDriveClient,
        node_uid: NodeUid,
        settings: ShareUrlSettings,
    ) -> anyhow::Result<UrlAccess> {
        let ctx = match Self::load_share_context(client, node_uid.clone()).await {
            Ok(ctx) => ctx,
            Err(_) => Self::create_share(client, node_uid.clone()).await?,
        };
        Self::create_public_link_on_share(client, &ctx, settings).await?;
        Self::decrypt_url_access(client, ctx.share_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("public link missing after create"))
    }

    pub async fn get_public_link_info(
        client: &ProtonDriveClient,
        url: &str,
    ) -> anyhow::Result<PublicLinkInfo> {
        let (token, password) = parse_public_link_url(url)?;
        let info = client.api().shares().get_public_link_info(&token).await?;
        Ok(PublicLinkInfo {
            token,
            password,
            is_custom_password_protected: (info.flags & 1) == 1,
        })
    }

    pub async fn authenticate_public_link(
        client: &ProtonDriveClient,
        url: &str,
    ) -> anyhow::Result<PublicLinkClient> {
        let (token, password) = parse_public_link_url(url)?;
        let info = client.api().shares().get_public_link_info(&token).await?;
        let version = SrpHashVersion::try_from(info.version)
            .map_err(|_| anyhow::anyhow!("unsupported SRP version {}", info.version))?;
        let auth = SRPAuth::new(
            &proton_srp::RPGPVerifier::default(),
            None,
            &password,
            version,
            &info.url_password_salt,
            &info.modulus,
            &info.server_ephemeral,
        )?;
        let proof: proton_srp::SRPProofB64 = auth.generate_proofs()?.into();
        let response = client
            .api()
            .shares()
            .auth_public_link(
                &token,
                PublicLinkAuthRequest {
                    client_proof: proof.client_proof.clone(),
                    client_ephemeral: proof.client_ephemeral.clone(),
                    srp_session: info.srp_session,
                },
            )
            .await?;
        if !proof.compare_server_proof(&response.server_proof) {
            anyhow::bail!("public link server proof mismatch");
        }
        let salt = STANDARD.decode(response.share.share_password_salt.as_bytes())?;
        let hashed = proton_srp::mailbox_password_hash(&password, &salt)?;
        let share_key = unlock_share_with_password(
            &response.share.share_key,
            &response.share.share_passphrase,
            hashed.as_bytes(),
        )?;
        Ok(PublicLinkClient {
            session: PublicLinkSession {
                token,
                session_uid: response.session_uid,
                access_token: response.access_token,
                root_uid: NodeUid::new(response.share.volume_id, response.share.link_id),
                share_key,
                public_role: response.share.public_permissions.to_role(),
            },
        })
    }

    pub async fn iterate_bookmarks(client: &ProtonDriveClient) -> anyhow::Result<Vec<Bookmark>> {
        let response = client.api().shares().list_bookmarks().await?;
        let mut out = Vec::new();
        let address = client.account().get_default_address().await?;
        let keys = client
            .account()
            .get_address_private_keys(&AddressId::new(address.address_id))
            .await?;
        let pgp_keys: Vec<PgpPrivateKey> = keys.into_iter().map(PgpPrivateKey).collect();
        let claim = crate::node::authorship::AuthorshipClaim {
            keys: vec![],
            author: crate::author::Author::ANONYMOUS,
            key_retrieval_error_message: None,
        };
        for bookmark in response.bookmarks {
            let mut password = String::new();
            if let Some(encrypted) = &bookmark.encrypted_url_password {
                if let Ok((bytes, _, _)) =
                    NodeCrypto::decrypt_message(encrypted, None, &pgp_keys, &claim)
                {
                    password = String::from_utf8(bytes).unwrap_or_default();
                }
            }
            let mut node_name = None;
            if let Some(name) = &bookmark.token.name {
                if let Ok((passphrase, _, _)) = NodeCrypto::decrypt_message(
                    &bookmark.token.share_passphrase,
                    None,
                    &pgp_keys,
                    &claim,
                ) {
                    if let Ok(share_key) = NodeCrypto::unlock_key_with_passphrase(
                        &bookmark.token.share_key,
                        &passphrase,
                    ) {
                        if let Ok((bytes, _, _)) =
                            NodeCrypto::decrypt_message(name, None, [&share_key], &claim)
                        {
                            node_name = String::from_utf8(bytes).ok();
                        }
                    }
                }
            }
            let url = if password.is_empty() {
                format!("https://drive.proton.me/urls/{}", bookmark.token.token)
            } else {
                format!(
                    "https://drive.proton.me/urls/{}#{}",
                    bookmark.token.token, password
                )
            };
            out.push(Bookmark {
                uid: bookmark.token.token.clone(),
                url,
                creation_time: bookmark.create_time,
                custom_password: None,
                node_name,
                media_type: bookmark.token.mime_type,
            });
        }
        Ok(out)
    }

    pub async fn create_bookmark(client: &ProtonDriveClient, url: &str) -> anyhow::Result<()> {
        let (token, password) = parse_public_link_url(url)?;
        let address = client.account().get_default_address().await?;
        let signing_key = client
            .account()
            .get_address_primary_private_key(&AddressId::new(address.address_id.clone()))
            .await?;
        let encryptor =
            Encryptor::default().with_encryption_key(signing_key.as_public_key());
        let encrypted = encryptor.encrypt(password.as_bytes())?;
        let armored = String::from_utf8(encrypted.armor()?)?;
        let address_key_id = address
            .keys
            .get(address.primary_key_index as usize)
            .map(|k| k.address_key_id.clone())
            .unwrap_or_default();
        client
            .api()
            .shares()
            .create_bookmark(
                &token,
                CreateBookmarkRequest {
                    bookmark_share_url: CreateBookmarkBody {
                        encrypted_url_password: armored,
                        address_id: address.address_id,
                        address_key_id,
                    },
                },
            )
            .await
    }

    pub async fn remove_bookmark(client: &ProtonDriveClient, bookmark_or_url: &str) -> anyhow::Result<()> {
        let token = if let Ok((token, _)) = parse_public_link_url(bookmark_or_url) {
            token
        } else {
            bookmark_or_url.to_string()
        };
        client.api().shares().delete_bookmark(&token).await
    }

    async fn node_share_id(
        client: &ProtonDriveClient,
        node_uid: NodeUid,
    ) -> anyhow::Result<Option<ShareId>> {
        let metadata = crate::node::operations::NodeOperations::get_node_metadata(client, node_uid)
            .await?
            .result()?;
        Ok(metadata.membership_share_id)
    }

    async fn node_secrets(
        client: &ProtonDriveClient,
        node_uid: NodeUid,
    ) -> anyhow::Result<NodeSecrets> {
        let metadata = crate::node::operations::NodeOperations::get_node_metadata(client, node_uid)
            .await?
            .result()?;
        Ok(match metadata.inner {
            NodeAndSecrets::File(_, s) => s.base,
            NodeAndSecrets::Folder(_, s) => s.base,
        })
    }

    async fn create_share(
        client: &ProtonDriveClient,
        node_uid: NodeUid,
    ) -> anyhow::Result<ShareContext> {
        let secrets = Self::node_secrets(client, node_uid.clone()).await?;
        let address = client.account().get_default_address().await?;
        let address_key = PgpPrivateKey(
            client
                .account()
                .get_address_primary_private_key(&AddressId::new(address.address_id.clone()))
                .await?,
        );
        let passphrase = CryptoGenerator::generate_passphrase();
        let share_key = CryptoGenerator::generate_private_key()?;
        let session_key = CryptoGenerator::generate_session_key();
        let sk = session_key.to_rpgp_sk()?;
        let encryptor = Encryptor::default()
            .with_session_key(sk)
            .with_encryption_keys([
                secrets.key.0.as_public_key(),
                address_key.0.as_public_key(),
            ])
            .with_signing_key(&address_key.0);
        let encrypted_passphrase = encryptor.encrypt(passphrase.as_bytes())?;
        let passphrase_signature = Signer::default()
            .with_signing_key(&address_key.0)
            .sign_detached(passphrase.as_bytes(), DataEncoding::Armored)?;
        let armored_key = share_key.to_armored_private_key(Some(passphrase.as_bytes()))?;
        let passphrase_key_packet = share_key.encrypt_session_key(&secrets.passphrase_session_key)?;
        let name_key_packet = share_key.encrypt_session_key(&secrets.name_session_key)?;
        let created = client
            .api()
            .shares()
            .create_standard_share(
                node_uid.volume_id.clone(),
                CreateShareRequest {
                    root_link_id: node_uid.link_id.raw().to_string(),
                    address_id: address.address_id.clone(),
                    name: "New Share".into(),
                    share_key: armored_key.0,
                    share_passphrase: String::from_utf8(encrypted_passphrase.armor()?)?,
                    share_passphrase_signature: String::from_utf8(passphrase_signature)?,
                    passphrase_key_packet: STANDARD.encode(passphrase_key_packet),
                    name_key_packet: STANDARD.encode(name_key_packet),
                },
            )
            .await?;
        Ok(ShareContext {
            share_id: created.share.id,
            address_id: AddressId::new(address.address_id),
            email: address.email_address,
            address_key,
            passphrase_session_key: session_key,
            editors_can_share: created.share.editors_can_share,
        })
    }

    async fn load_share_context(
        client: &ProtonDriveClient,
        node_uid: NodeUid,
    ) -> anyhow::Result<ShareContext> {
        let share_id = Self::node_share_id(client, node_uid.clone())
            .await?
            .ok_or_else(|| anyhow::anyhow!("node is not shared"))?;
        let secrets = Self::node_secrets(client, node_uid).await?;
        let response = client.api().shares().get_share(share_id.clone()).await?;
        let claim = crate::node::authorship::AuthorshipClaim {
            keys: vec![],
            author: crate::author::Author::ANONYMOUS,
            key_retrieval_error_message: None,
        };
        let decrypted = NodeCrypto::decrypt_message(
            &response.passphrase,
            Some(&response.passphrase_signature),
            [&secrets.key],
            &claim,
        );
        let (_passphrase, session_key, _) = match decrypted {
            Ok(v) => v,
            Err(_) => {
                let address_keys = client
                    .account()
                    .get_address_private_keys(&response.address_id)
                    .await?
                    .into_iter()
                    .map(PgpPrivateKey)
                    .collect::<Vec<_>>();
                NodeCrypto::decrypt_message(
                    &response.passphrase,
                    Some(&response.passphrase_signature),
                    &address_keys,
                    &claim,
                )
                .map_err(|e| anyhow::anyhow!(e))?
            }
        };
        let session_key = session_key
            .map(|sk| PgpSessionKey {
                algorithm: sk.algorithm().map(u8::from).unwrap_or(9),
                key: sk.as_ref().to_vec(),
            })
            .unwrap_or_else(CryptoGenerator::generate_session_key);
        let address = client.account().get_address(&response.address_id).await?;
        let address_key = PgpPrivateKey(
            client
                .account()
                .get_address_primary_private_key(&response.address_id)
                .await?,
        );
        Ok(ShareContext {
            share_id,
            address_id: response.address_id,
            email: address.email_address,
            address_key,
            passphrase_session_key: session_key,
            editors_can_share: response.editors_can_share,
        })
    }

    async fn invite_proton(
        client: &ProtonDriveClient,
        ctx: &ShareContext,
        email: &str,
        role: MemberRole,
        external_invitation_id: Option<&str>,
        message: Option<String>,
        node_name: Option<String>,
    ) -> anyhow::Result<ProtonInvitation> {
        let public_keys = client.account().get_address_public_keys(email).await?;
        let public_key = public_keys
            .first()
            .ok_or_else(|| anyhow::anyhow!("invitee has no public keys"))?;
        let (key_packet, key_packet_signature) =
            encrypt_invitation(&ctx.passphrase_session_key, public_key, &ctx.address_key)?;
        let response = client
            .api()
            .shares()
            .invite_proton_user(
                ctx.share_id.clone(),
                InviteProtonUserRequest {
                    invitation: InviteProtonUserBody {
                        inviter_email: ctx.email.clone(),
                        invitee_email: email.to_string(),
                        permissions: ShareMemberPermissions::from_role(role).bits(),
                        key_packet,
                        key_packet_signature,
                        external_invitation_id: external_invitation_id.map(str::to_string),
                    },
                    email_details: EmailDetailsBody { message, item_name: node_name },
                },
            )
            .await?;
        Ok(ProtonInvitation {
            uid: invitation_uid(&ctx.share_id, &response.invitation.invitation_id),
            invitee_email: response.invitation.invitee_email,
            added_by_email: response.invitation.inviter_email,
            role: response.invitation.permissions.to_role(),
            invitation_time: response.invitation.create_time,
        })
    }

    async fn invite_external(
        client: &ProtonDriveClient,
        ctx: &ShareContext,
        email: &str,
        role: MemberRole,
        message: Option<String>,
        node_name: Option<String>,
    ) -> anyhow::Result<NonProtonInvitation> {
        let payload = format!("{}|{}", email, STANDARD.encode(&ctx.passphrase_session_key.key));
        let context = SignatureContext::new(SIGNING_EXTERNAL.to_string(), true);
        let signature = Signer::default()
            .with_signing_key(&ctx.address_key.0)
            .with_signature_context(context)
            .sign_detached(payload.as_bytes(), DataEncoding::Unarmored)?;
        let response = client
            .api()
            .shares()
            .invite_external_user(
                ctx.share_id.clone(),
                InviteExternalUserRequest {
                    invitation: InviteExternalUserBody {
                        inviter_address_id: ctx.address_id.raw().to_string(),
                        invitee_email: email.to_string(),
                        permissions: ShareMemberPermissions::from_role(role).bits(),
                        signature: STANDARD.encode(signature),
                    },
                    email_details: EmailDetailsBody { message, item_name: node_name },
                },
            )
            .await?;
        Ok(NonProtonInvitation {
            uid: invitation_uid(&ctx.share_id, &response.invitation.invitation_id),
            invitee_email: response.invitation.invitee_email,
            added_by_email: response.invitation.inviter_email,
            role: response.invitation.permissions.to_role(),
            state: if response.invitation.state == 1 {
                NonProtonInvitationState::Pending
            } else {
                NonProtonInvitationState::UserRegistered
            },
            invitation_time: response.invitation.create_time,
        })
    }

    async fn create_public_link_on_share(
        client: &ProtonDriveClient,
        ctx: &ShareContext,
        settings: ShareUrlSettings,
    ) -> anyhow::Result<()> {
        if settings.role == MemberRole::Admin {
            anyhow::bail!("Cannot set admin role for URL access.");
        }
        let generated = random_password(PUBLIC_LINK_PASSWORD_LEN);
        let password = match &settings.custom_password {
            Some(custom) => format!("{generated}{custom}"),
            None => generated,
        };
        let includes_custom = settings.custom_password.is_some();
        let modulus = client.api().shares().get_srp_modulus().await?;
        let verifier = SRPAuth::generate_verifier_with_pgp(&password, None, &modulus.modulus)?;
        let verifier_b64: SRPVerifierB64 = verifier.into();
        let mut salt_bytes = [0u8; 16];
        rand::rng().fill(&mut salt_bytes);
        let salt = STANDARD.encode(salt_bytes);
        let salted = proton_srp::mailbox_password_hash(&password, &salt_bytes)?;
        let key_packet = Encryptor::default()
            .with_passphrase(salted.as_bytes())
            .encrypt_session_key(&ctx.passphrase_session_key.to_rpgp_sk()?)?;
        let encryptor =
            Encryptor::default().with_encryption_key(ctx.address_key.0.as_public_key());
        let armored_password = String::from_utf8(encryptor.encrypt(password.as_bytes())?.armor()?)?;
        client
            .api()
            .shares()
            .create_share_url(
                ctx.share_id.clone(),
                CreateShareUrlRequest {
                    creator_email: ctx.email.clone(),
                    permissions: ShareMemberPermissions::from_role(settings.role).bits(),
                    flags: if includes_custom { 3 } else { 2 },
                    expiration_time: settings.expiration.map(|t| t.timestamp()),
                    share_password_salt: salt,
                    share_passphrase_key_packet: STANDARD.encode(key_packet),
                    password: armored_password,
                    url_password_salt: verifier_b64.salt,
                    srp_verifier: verifier_b64.verifier,
                    srp_modulus_id: modulus.modulus_id,
                    max_accesses: 0,
                },
            )
            .await?;
        Ok(())
    }

    async fn decrypt_url_access(
        client: &ProtonDriveClient,
        share_id: ShareId,
    ) -> anyhow::Result<Option<UrlAccess>> {
        let urls = client.api().shares().get_share_urls(share_id.clone()).await?;
        let Some(url) = urls.share_urls.into_iter().next() else {
            return Ok(None);
        };
        let address = client
            .account()
            .get_address_private_keys(&AddressId::new(
                client.account().get_default_address().await?.address_id,
            ))
            .await?;
        let pgp_keys: Vec<PgpPrivateKey> = address.into_iter().map(PgpPrivateKey).collect();
        let claim = crate::node::authorship::AuthorshipClaim {
            keys: vec![],
            author: crate::author::Author::ANONYMOUS,
            key_retrieval_error_message: None,
        };
        let password = NodeCrypto::decrypt_message(&url.password, None, &pgp_keys, &claim)
            .ok()
            .and_then(|(b, _, _)| String::from_utf8(b).ok())
            .unwrap_or_default();
        let (url_password, custom_password) = split_generated_and_custom(&password, url.flags);
        Ok(Some(UrlAccess {
            uid: invitation_uid(&share_id, &url.share_url_id),
            url: format!("{}#{url_password}", url.public_url),
            role: url.permissions.to_role(),
            creation_time: url.create_time,
            expiration_time: url
                .expiration_time
                .and_then(|s| DateTime::from_timestamp(s, 0)),
            custom_password,
            number_of_initialized_downloads: url.num_accesses,
        }))
    }
}

struct ShareContext {
    share_id: ShareId,
    address_id: AddressId,
    email: String,
    address_key: PgpPrivateKey,
    passphrase_session_key: PgpSessionKey,
    editors_can_share: bool,
}

fn encrypt_invitation(
    session_key: &PgpSessionKey,
    invitee: &PublicKey,
    inviter: &PgpPrivateKey,
) -> anyhow::Result<(String, String)> {
    let sk = session_key.to_rpgp_sk()?;
    let key_packet = Encryptor::default()
        .with_encryption_key(invitee)
        .encrypt_session_key(&sk)?;
    let context = SignatureContext::new(SIGNING_INVITER.to_string(), true);
    let signature = Signer::default()
        .with_signing_key(&inviter.0)
        .with_signature_context(context)
        .sign_detached(&key_packet, DataEncoding::Unarmored)?;
    Ok((STANDARD.encode(key_packet), STANDARD.encode(signature)))
}

fn unlock_share_with_password(
    armored_key: &crate::pgp::PgpArmoredPrivateKey,
    armored_passphrase: &crate::pgp::PgpArmoredMessage,
    password: &[u8],
) -> anyhow::Result<PgpPrivateKey> {
    let decryptor = proton_rpgp::Decryptor::default().with_passphrase(password);
    let unarmored = if armored_passphrase.0.contains("-----BEGIN PGP MESSAGE-----") {
        proton_rpgp::armor::unarmor(armored_passphrase.0.as_bytes())?
    } else {
        armored_passphrase.0.as_bytes().to_vec()
    };
    let passphrase = decryptor.decrypt(&unarmored, DataEncoding::Auto)?;
    NodeCrypto::unlock_key_with_passphrase(armored_key, &passphrase.data)
        .or_else(|_| NodeCrypto::unlock_key_with_passphrase(armored_key, password))
        .map_err(|e| anyhow::anyhow!(e))
}

fn random_password(len: usize) -> String {
    (0..len)
        .map(|_| {
            let idx = rand::random_range(0..GENERATED_PASSWORD_CHARSET.len());
            GENERATED_PASSWORD_CHARSET[idx] as char
        })
        .collect()
}

fn split_generated_and_custom(password: &str, flags: u32) -> (String, Option<String>) {
    match flags {
        3 if password.len() > PUBLIC_LINK_PASSWORD_LEN => {
            let (generated, custom) = password.split_at(PUBLIC_LINK_PASSWORD_LEN);
            (generated.to_string(), Some(custom.to_string()))
        }
        _ => (password.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sharing_uid_round_trip() {
        let uid = invitation_uid(&ShareId::new("share".into()), "inv");
        assert_eq!(uid, "share~inv");
        let (share, inv) = split_sharing_uid(&uid).unwrap();
        assert_eq!((share, inv), ("share", "inv"));
    }

    #[test]
    fn public_link_url_parse() {
        let (token, password) =
            parse_public_link_url("https://drive.proton.me/urls/abcTOKEN#s3cret").unwrap();
        assert_eq!(token, "abcTOKEN");
        assert_eq!(password, "s3cret");
    }

    #[test]
    fn member_role_permissions() {
        assert_eq!(ShareMemberPermissions::from_role(MemberRole::Viewer).bits(), 4);
        assert_eq!(ShareMemberPermissions::from_role(MemberRole::Editor).bits(), 6);
        assert_eq!(ShareMemberPermissions::from_role(MemberRole::Admin).bits(), 22);
    }
}
