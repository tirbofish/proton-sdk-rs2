use crate::client::ProtonDriveClient;
use crate::node::revision::RevisionUid;
use crate::pgp::PgpSessionKey;
use sha2::Digest;
use tokio::io::{AsyncWrite, AsyncWriteExt};

#[derive(Clone)]
pub struct BlockDownloader {
    // todo: figure out why its here
    #[allow(dead_code)]
    max_degree_of_parallelism: usize,
    pub queue: crate::node::transfer::TransferQueue,
}

impl BlockDownloader {
    pub fn new(max_degree_of_parallelism: usize) -> Self {
        Self {
            max_degree_of_parallelism,
            queue: crate::node::transfer::TransferQueue::new(max_degree_of_parallelism),
        }
    }

    pub async fn download(
        &self,
        client: &ProtonDriveClient,
        revision_uid: RevisionUid,
        index: i32,
        bare_url: String,
        token: String,
        content_key: PgpSessionKey,
        output_stream: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> anyhow::Result<Vec<u8>> {
        let mut response = client
            .api()
            .storage()
            .get_blob_stream(&bare_url, &token)
            .await?;
        let bytes = response.bytes().await?;

        let mut hasher = sha2::Sha256::new();
        sha2::Digest::update(&mut hasher, &bytes);

        let alg = proton_rpgp::pgp::crypto::sym::SymmetricKeyAlgorithm::from(content_key.algorithm);
        let sk = proton_rpgp::SessionKey::new(&content_key.key, alg);

        let result = proton_rpgp::Decryptor::default()
            .with_session_key(sk)
            .decrypt(&bytes, proton_rpgp::DataEncoding::Auto)?;

        output_stream.write_all(&result.data).await?;

        Ok(sha2::Digest::finalize(hasher).to_vec())
    }
}
