use crate::client::ProtonDriveClient;
use crate::node::revision::RevisionUid;
use crate::pgp::PgpSessionKey;
use sha2::Digest;
use tokio::io::{AsyncWrite, AsyncWriteExt};

#[derive(Clone)]
pub struct BlockDownloader {
    pub queue: crate::node::transfer::TransferQueue,
}

impl BlockDownloader {
    pub fn new(max_degree_of_parallelism: usize) -> Self {
        Self {
            queue: crate::node::transfer::TransferQueue::new(max_degree_of_parallelism),
        }
    }

    pub async fn download(
        &self,
        client: &ProtonDriveClient,
        _revision_uid: RevisionUid,
        _index: i32,
        bare_url: String,
        token: String,
        content_key: PgpSessionKey,
        output_stream: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> anyhow::Result<Vec<u8>> {
        let response = client
            .api()
            .storage()
            .get_blob_stream(&bare_url, &token)
            .await?;
        let bytes = response.bytes().await?;

        let mut hasher = sha2::Sha256::new();
        sha2::Digest::update(&mut hasher, &bytes);

        output_stream.write_all(&content_key.decrypt(&bytes)?).await?;

        Ok(sha2::Digest::finalize(hasher).to_vec())
    }
}
