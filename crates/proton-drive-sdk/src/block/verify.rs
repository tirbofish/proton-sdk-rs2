use reqwest_middleware::ClientWithMiddleware;
use reqwest::Url;
use proton_sdk_rs2::auth::TokenCredential;
use crate::pgp::{PgpSessionKey, PgpPrivateKey};
use crate::node::revision::RevisionUid;
use crate::api::block::verification::{BlockVerificationApiClient, DefaultBlockVerificationApiClient};

#[derive(Clone)]
pub struct BlockVerifier {
    _session_key: PgpSessionKey,
    verification_code: Vec<u8>,
}

impl BlockVerifier {
    pub fn new(session_key: PgpSessionKey, verification_code: Vec<u8>) -> Self {
        Self { _session_key: session_key, verification_code }
    }

    pub fn data_packet_prefix_max_length(&self) -> usize {
        self.verification_code.len()
    }

    pub fn verify_block(
        &self,
        data_packet_prefix: &[u8],
        _plain_data_prefix: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        Ok(self.create_verification_token(data_packet_prefix))
    }

    fn create_verification_token(&self, data_packet_prefix: &[u8]) -> Vec<u8> {
        let length = self.verification_code.len();
        let mut data_packet_prefix_for_token = vec![0u8; length];
        let copy_len = std::cmp::min(data_packet_prefix.len(), length);
        data_packet_prefix_for_token[..copy_len].copy_from_slice(&data_packet_prefix[..copy_len]);

        let mut token_data = vec![0u8; length];
        for i in 0..length {
            token_data[i] = self.verification_code[i] ^ data_packet_prefix_for_token[i];
        }
        token_data
    }
}

#[async_trait::async_trait]
pub trait BlockVerifierFactory: Send + Sync {
    async fn create(
        &self,
        revision_uid: RevisionUid,
        key: &PgpPrivateKey,
    ) -> anyhow::Result<BlockVerifier>;
}

pub struct DefaultBlockVerifierFactory {
    api_client: Box<dyn BlockVerificationApiClient>,
}

impl DefaultBlockVerifierFactory {
    pub fn new(
        client: ClientWithMiddleware,
        base_url: Url,
        token_credential: Option<TokenCredential>,
    ) -> Self {
        Self {
            api_client: Box::new(DefaultBlockVerificationApiClient::new(
                client,
                base_url,
                token_credential,
            )),
        }
    }
}

#[async_trait::async_trait]
impl BlockVerifierFactory for DefaultBlockVerifierFactory {
    async fn create(
        &self,
        revision_uid: RevisionUid,
        key: &PgpPrivateKey,
    ) -> anyhow::Result<BlockVerifier> {
        let verification_input = self.api_client.get_verification_input(
            &revision_uid.node_uid.volume_id,
            &revision_uid.node_uid.link_id,
            &revision_uid.revision_id,
        ).await?;

        let session_key = key.decrypt_session_key(&verification_input.content_key_packet)
            .map_err(|e| anyhow::anyhow!("Node key and session key mismatch: {}", e))?;

        Ok(BlockVerifier::new(
            session_key,
            verification_input.verification_code.unwrap_or_default(),
        ))
    }
}
