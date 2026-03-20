use crate::block::verify::BlockVerifier;
use crate::client::ProtonDriveClient;
use crate::node::NodeUid;
use crate::node::revision::RevisionUid;
use crate::pgp::{PgpArmoredSignature, PgpPrivateKey, PgpSessionKey};
use crate::revision::RevisionId;
use async_trait::async_trait;
use proton_rpgp::{AsPublicKeyRef, DataEncoding, Encryptor, Signer};
use proton_sdk_rs2::protobuf::Address;
use std::sync::Arc;

pub struct RevisionDraft {
    pub uid: RevisionUid,
    pub file_key: PgpPrivateKey,
    pub content_key: PgpSessionKey,
    pub signing_key: PgpPrivateKey,
    pub hash_key: Option<Vec<u8>>,
    pub membership_address: Address,
    pub block_verifier: BlockVerifier,
}

pub struct NewFileDraftProvider {
    pub client: Arc<ProtonDriveClient>,
    pub parent_folder_uid: NodeUid,
    pub name: String,
    pub media_type: String,
    pub override_existing_draft_by_other_client: bool,
}

pub struct NewRevisionDraftProvider {
    pub client: Arc<ProtonDriveClient>,
    pub node_uid: NodeUid,
    pub revision_id: RevisionId,
}

#[async_trait]
pub trait RevisionDraftProvider: Send + Sync {
    async fn get_draft(&self) -> anyhow::Result<RevisionDraft>;
}

#[async_trait]
impl RevisionDraftProvider for NewFileDraftProvider {
    async fn get_draft(&self) -> anyhow::Result<RevisionDraft> {
        let parent_secrets = crate::node::folder::FolderOperations::get_secrets(
            &self.client,
            self.parent_folder_uid.clone(),
        )
        .await?;

        // Find membership address (simplified for now, use default)
        let membership_address = self.client.account().get_default_address().await?;
        let signing_key = self
            .client
            .account()
            .get_address_primary_private_key(&crate::account::AddressId::new(
                membership_address.address_id.clone(),
            ))
            .await?;

        // Create draft via API
        let node_key = crate::crypto::CryptoGenerator::generate_private_key()?;
        let folder_passphrase = crate::crypto::CryptoGenerator::generate_passphrase();
        let locked_folder_key =
            node_key.to_armored_private_key(Some(folder_passphrase.as_bytes()))?;

        let (encrypted_passphrase, passphrase_signature, _passphrase_session_key) =
            crate::node::crypto::NodeCrypto::encrypt_and_sign_passphrase(
                folder_passphrase.as_bytes(),
                &parent_secrets.base.key,
                &PgpPrivateKey(signing_key.clone()),
            )?;

        let (encrypted_name, name_hash_digest, _name_session_key) =
            crate::node::crypto::NodeCrypto::encrypt_and_sign_name(
                &self.name,
                &parent_secrets.hash_key,
                &parent_secrets.base.key,
                &PgpPrivateKey(signing_key.clone()),
            )?;

        let content_key = crate::crypto::CryptoGenerator::generate_session_key();

        let request = crate::api::file::FileCreationRequest {
            base: crate::api::node::NodeCreationRequest {
                name: encrypted_name,
                name_hash_digest,
                parent_link_id: self.parent_folder_uid.link_id.clone(),
                passphrase: encrypted_passphrase,
                passphrase_signature: passphrase_signature
                    .ok_or_else(|| anyhow::anyhow!("Passphrase signature missing"))?,
                key: locked_folder_key,
            },
            media_type: self.media_type.clone(),
            content_key_packet: Encryptor::default()
                .with_encryption_key(node_key.0.as_public_key())
                .encrypt_session_key(&content_key.to_rpgp_sk()?)?,
            content_key_signature: PgpArmoredSignature(String::from_utf8(
                Signer::default()
                    .with_signing_key(&PgpPrivateKey(signing_key.clone()).0)
                    .sign_detached(&content_key.key, DataEncoding::Armored)?,
            )?),
            signature_email_address: membership_address.email_address.clone(),
            client_uid: None,
            intended_upload_size: None,
        };

        let response = self
            .client
            .api()
            .files()
            .create_file(self.parent_folder_uid.volume_id.clone(), request)
            .await?;

        let draft_node_uid = NodeUid::new(
            self.parent_folder_uid.volume_id.clone(),
            response.identifiers.link_id,
        );
        let draft_revision_uid = RevisionUid::new(draft_node_uid, response.identifiers.revision_id);

        let block_verifier = self
            .client
            .block_verifier_factory()
            .create(
                draft_revision_uid.clone(),
                &PgpPrivateKey(node_key.0.clone()),
            )
            .await?;

        Ok(RevisionDraft {
            uid: draft_revision_uid,
            file_key: PgpPrivateKey(node_key.0),
            content_key,
            signing_key: PgpPrivateKey(signing_key),
            hash_key: Some(parent_secrets.hash_key),
            membership_address,
            block_verifier,
        })
    }
}

#[async_trait]
impl RevisionDraftProvider for NewRevisionDraftProvider {
    async fn get_draft(&self) -> anyhow::Result<RevisionDraft> {
        // Implementation for new revision draft
        let node_secrets =
            crate::node::file::FileOperations::get_secrets(&self.client, self.node_uid.clone())
                .await?;
        let membership_address = self.client.account().get_default_address().await?;
        let signing_key = self
            .client
            .account()
            .get_address_primary_private_key(&crate::account::AddressId::new(
                membership_address.address_id.clone(),
            ))
            .await?;

        let content_key = crate::crypto::CryptoGenerator::generate_session_key();

        let request = crate::api::revision::RevisionCreationRequest {
            current_revision_id: Some(self.revision_id.clone()),
            client_id: Some(self.client.uid().to_string()),
        };

        let response = self
            .client
            .api()
            .files()
            .create_revision(
                self.node_uid.volume_id.clone(),
                self.node_uid.link_id.clone(),
                request,
            )
            .await?;

        let draft_revision_uid =
            RevisionUid::new(self.node_uid.clone(), response.identity.revision_id);

        let block_verifier = self
            .client
            .block_verifier_factory()
            .create(draft_revision_uid.clone(), &node_secrets.base.key)
            .await?;

        Ok(RevisionDraft {
            uid: draft_revision_uid,
            file_key: node_secrets.base.key,
            content_key,
            signing_key: PgpPrivateKey(signing_key),
            hash_key: None,
            membership_address,
            block_verifier,
        })
    }
}
