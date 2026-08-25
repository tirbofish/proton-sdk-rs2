use crate::client::ProtonDriveClient;
use crate::node::draft::RevisionDraft;
use crate::pgp::PgpArmoredMessage;
use proton_rpgp::{AsPublicKeyRef, pgp::ser::Serialize};
use sha2::Digest;

pub struct BlockUploadResult {
    pub size: usize,
    pub sha256_digest: Vec<u8>,
}

/// Holds pre-encrypted block data ready for upload.
/// This allows separating encryption (CPU-bound) from upload (I/O-bound).
pub struct EncryptedBlock {
    pub block_number: i32,
    pub plain_size: usize,
    pub encrypted_data: Vec<u8>,
    pub sha256_digest: Vec<u8>,
    pub encrypted_signature: PgpArmoredMessage,
    pub verification_token: Vec<u8>,
}

#[derive(Clone)]
pub struct BlockUploader {
    pub queue: crate::node::transfer::TransferQueue,
}

impl BlockUploader {
    pub fn new(max_degree_of_parallelism: usize) -> Self {
        Self {
            queue: crate::node::transfer::TransferQueue::new(max_degree_of_parallelism),
        }
    }

    /// Encrypts a block of data without uploading it.
    /// This is the CPU-bound portion that can be pipelined ahead of uploads.
    #[tracing::instrument(skip(self, draft, plain_data))]
    pub fn encrypt_block(
        &self,
        draft: &RevisionDraft,
        block_number: i32,
        plain_data: &[u8],
    ) -> anyhow::Result<EncryptedBlock> {
        tracing::debug!(block_number, size = plain_data.len(), "Encrypting block");
        let plain_data_len = plain_data.len();

        // 1. Sign the plain data
        let signer = proton_rpgp::Signer::default().with_signing_key(&draft.signing_key.0);
        let signature = signer.sign_detached(plain_data, proton_rpgp::DataEncoding::Auto)?;

        // 2. Encrypt the signature using the file key (unsigned, matching official SDK)
        let signature_encryptor =
            proton_rpgp::Encryptor::default().with_encryption_key(draft.file_key.0.as_public_key());
        let signature_result = signature_encryptor.encrypt(&signature)?;
        let encrypted_signature = PgpArmoredMessage(String::from_utf8(signature_result.armor()?)?);

        // 3. Encrypt the content using the session key
        let sk = draft.content_key.to_rpgp_sk()?;
        let content_encryptor = proton_rpgp::Encryptor::default().with_session_key(sk);

        let content_result = content_encryptor.encrypt(plain_data)?;
        let encrypted_data = content_result.to_bytes()?;

        // 4. Compute SHA256 of the ENCRYPTED data
        let mut hasher = sha2::Sha256::new();
        hasher.update(&encrypted_data);
        let sha256_digest = hasher.finalize().to_vec();

        // 5. Block Verification
        const AEAD_CHUNK_SIZE: usize = 1 + 1 + 4 + 32 + (1 << 17) + 1 + 36 + 16;
        let prefix_len = std::cmp::min(encrypted_data.len(), AEAD_CHUNK_SIZE);

        let verification_token = draft.block_verifier.verify_block(
            &encrypted_data[..prefix_len],
            &plain_data[..std::cmp::min(
                plain_data.len(),
                draft.block_verifier.data_packet_prefix_max_length(),
            )],
        )?;

        Ok(EncryptedBlock {
            block_number,
            plain_size: plain_data_len,
            encrypted_data,
            sha256_digest,
            encrypted_signature,
            verification_token,
        })
    }

    /// Uploads a pre-encrypted block to storage.
    /// This is the I/O-bound portion.
    #[tracing::instrument(skip(self, client, draft, encrypted_block))]
    pub async fn upload_encrypted_block(
        &self,
        client: &ProtonDriveClient,
        draft: &RevisionDraft,
        encrypted_block: EncryptedBlock,
    ) -> anyhow::Result<BlockUploadResult> {
        let _permit = self.queue.start_block().await?;
        tracing::debug!(
            block_number = encrypted_block.block_number,
            encrypted_size = encrypted_block.encrypted_data.len(),
            "Uploading pre-encrypted block"
        );

        // Prepare Block Upload
        let address = client.account().get_default_address().await?;
        let request = crate::api::block::BlockUploadPreparationRequest {
            address_id: crate::account::AddressId::new(address.address_id),
            volume_id: draft.uid.node_uid.volume_id.clone(),
            link_id: draft.uid.node_uid.link_id.clone(),
            revision_id: draft.uid.revision_id.clone(),
            blocks: vec![crate::api::block::BlockCreationRequest {
                index: encrypted_block.block_number,
                size: encrypted_block.encrypted_data.len() as i32,
                encrypted_signature: encrypted_block.encrypted_signature,
                hash_digest: encrypted_block.sha256_digest.clone(),
                verification_output: crate::api::block::verification::BlockVerificationOutput {
                    token: encrypted_block.verification_token,
                },
            }],
            thumbnails: vec![],
        };

        let response = client.api().files().prepare_block_upload(request).await?;
        let target = response
            .upload_targets
            .get(0)
            .ok_or_else(|| anyhow::anyhow!("No upload target returned"))?;

        // Upload Blob
        client
            .api()
            .storage()
            .upload_blob(
                &target.bare_url,
                &target.token,
                encrypted_block.encrypted_data.into(),
            )
            .await?;

        Ok(BlockUploadResult {
            size: encrypted_block.plain_size,
            sha256_digest: encrypted_block.sha256_digest,
        })
    }

    #[tracing::instrument(skip(self, client, draft, plain_data, on_block_progress))]
    pub async fn upload_content(
        &self,
        client: &ProtonDriveClient,
        draft: &RevisionDraft,
        block_number: i32,
        plain_data: &[u8],
        on_block_progress: Option<&Box<dyn Fn(i64) + Send + Sync>>,
    ) -> anyhow::Result<BlockUploadResult> {
        let _permit = self.queue.start_block().await?;
        tracing::debug!(block_number, size = plain_data.len(), "Uploading block");
        let plain_data_len = plain_data.len();

        // 1. Sign the plain data
        let signer = proton_rpgp::Signer::default().with_signing_key(&draft.signing_key.0);
        let signature = signer.sign_detached(plain_data, proton_rpgp::DataEncoding::Auto)?;

        // 2. Encrypt the signature using the file key (unsigned, matching official SDK)
        let signature_encryptor =
            proton_rpgp::Encryptor::default().with_encryption_key(draft.file_key.0.as_public_key());
        let signature_result = signature_encryptor.encrypt(&signature)?;
        let encrypted_signature =
            crate::pgp::PgpArmoredMessage(String::from_utf8(signature_result.armor()?)?);

        // 3. Encrypt the content using the session key
        let sk = draft.content_key.to_rpgp_sk()?;
        let content_encryptor = proton_rpgp::Encryptor::default().with_session_key(sk);

        let content_result = content_encryptor.encrypt(plain_data)?;
        let encrypted_data = content_result.to_bytes()?;

        // 4. Compute SHA256 of the ENCRYPTED data
        let mut hasher = sha2::Sha256::new();
        hasher.update(&encrypted_data);
        let sha256_digest = hasher.finalize().to_vec();

        // 5. Block Verification
        const AEAD_CHUNK_SIZE: usize = 1 + 1 + 4 + 32 + (1 << 17) + 1 + 36 + 16;
        let prefix_len = std::cmp::min(encrypted_data.len(), AEAD_CHUNK_SIZE);

        let verification_token = draft.block_verifier.verify_block(
            &encrypted_data[..prefix_len],
            &plain_data[..std::cmp::min(
                plain_data.len(),
                draft.block_verifier.data_packet_prefix_max_length(),
            )],
        )?;

        // 6. Prepare Block Upload
        let address = client.account().get_default_address().await?;
        let request = crate::api::block::BlockUploadPreparationRequest {
            address_id: crate::account::AddressId::new(address.address_id),
            volume_id: draft.uid.node_uid.volume_id.clone(),
            link_id: draft.uid.node_uid.link_id.clone(),
            revision_id: draft.uid.revision_id.clone(),
            blocks: vec![crate::api::block::BlockCreationRequest {
                index: block_number,
                size: encrypted_data.len() as i32,
                encrypted_signature,
                hash_digest: sha256_digest.clone(),
                verification_output: crate::api::block::verification::BlockVerificationOutput {
                    token: verification_token,
                },
            }],
            thumbnails: vec![],
        };

        let response = client.api().files().prepare_block_upload(request).await?;
        let target = response
            .upload_targets
            .get(0)
            .ok_or_else(|| anyhow::anyhow!("No upload target returned"))?;

        // 7. Upload Blob
        client
            .api()
            .storage()
            .upload_blob(&target.bare_url, &target.token, encrypted_data.into())
            .await?;

        if let Some(on_progress) = on_block_progress {
            on_progress(plain_data_len as i64);
        }

        Ok(BlockUploadResult {
            size: plain_data_len,
            sha256_digest,
        })
    }
}
