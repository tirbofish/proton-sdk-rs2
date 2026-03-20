use crate::client::ProtonDriveClient;
use crate::node::draft::RevisionDraft;
use proton_rpgp::{AsPublicKeyRef, pgp::ser::Serialize};
use sha2::Digest;

pub struct BlockUploadResult {
    pub size: usize,
    pub sha256_digest: Vec<u8>,
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

    #[tracing::instrument(skip(self, client, draft, plain_data, on_block_progress))]
    pub async fn upload_content(
        &self,
        client: &ProtonDriveClient,
        draft: &RevisionDraft,
        block_number: i32,
        plain_data: &[u8],
        on_block_progress: Option<&Box<dyn Fn(i64) + Send + Sync>>,
    ) -> anyhow::Result<BlockUploadResult> {
        tracing::debug!(block_number, size = plain_data.len(), "Uploading block");
        let plain_data_len = plain_data.len();

        // 1. Sign the plain data
        let signer = proton_rpgp::Signer::default().with_signing_key(&draft.signing_key.0);
        let signature = signer.sign_detached(plain_data, proton_rpgp::DataEncoding::Auto)?;

        // 2. Encrypt the signature using the file key
        let signature_encryptor = proton_rpgp::Encryptor::default()
            .with_encryption_key(draft.file_key.0.as_public_key())
            .with_signing_key(&draft.signing_key.0);
        let signature_result = signature_encryptor.encrypt(&signature)?;
        let encrypted_signature =
            crate::pgp::PgpArmoredMessage(String::from_utf8(signature_result.armor()?)?);

        // 3. Encrypt the content using the session key
        let sk = draft.content_key.to_rpgp_sk()?;
        let content_encryptor = proton_rpgp::Encryptor::default()
            .with_session_key(sk)
            .with_signing_key(&draft.signing_key.0);

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
