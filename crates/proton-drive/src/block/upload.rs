use sha2::Digest;
use std::sync::Arc;
use crate::client::ProtonDriveClient;
use crate::node::revision::RevisionUid;
use crate::pgp::PgpSessionKey;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

pub struct BlockUploadResult {
    pub size: usize,
    pub sha256_digest: Vec<u8>,
}

#[derive(Clone)]
pub struct BlockUploader {
    max_degree_of_parallelism: usize,
    pub queue: crate::node::transfer::TransferQueue,
}

impl BlockUploader {
    pub fn new(max_degree_of_parallelism: usize) -> Self {
        Self {
            max_degree_of_parallelism,
            queue: crate::node::transfer::TransferQueue::new(max_degree_of_parallelism),
        }
    }

    pub async fn upload_content(
        &self,
        client: &ProtonDriveClient,
        _revision_uid: RevisionUid,
        _block_number: i32,
        plain_data_stream: &mut (dyn AsyncRead + Unpin + Send),
        _content_key: &PgpSessionKey,
        _signing_key: &crate::pgp::PgpPrivateKey,
    ) -> anyhow::Result<BlockUploadResult> {
        let mut data = Vec::new();
        plain_data_stream.read_to_end(&mut data).await?;
        
        let size = data.len();
        let mut hasher = sha2::Sha256::new();
        sha2::Digest::update(&mut hasher, &data);
        let sha256_digest = sha2::Digest::finalize(hasher).to_vec();

        // TODO: Port streaming encryption and signing logic from BlockUploader.cs
        // This requires PgpSessionKey encryption support which we need to verify in pgp crate
        
        Ok(BlockUploadResult {
            size,
            sha256_digest,
        })
    }
}
