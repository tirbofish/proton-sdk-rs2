use crate::api::block::BlockListingRevisionDto;
use crate::client::ProtonDriveClient;
use crate::node::revision::RevisionUid;
use crate::node::transfer::TransferQueue;
use crate::pgp::{PgpPrivateKey, PgpSessionKey};
use log::{debug, info};
use proton_rpgp::{DataEncoding, Decryptor, SessionKey, pgp::crypto::sym::SymmetricKeyAlgorithm};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::sync::watch;

pub const MIN_BLOCK_INDEX: i32 = 1;
pub const DEFAULT_BLOCK_PAGE_SIZE: i32 = 10;

#[derive(Debug, thiserror::Error)]
#[error("File authenticity check failed: {message}")]
pub struct CompletedDownloadManifestVerificationException {
    pub message: String,
}

impl CompletedDownloadManifestVerificationException {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Data integrity error: {message}")]
pub struct DataIntegrityException {
    pub message: String,
}

impl DataIntegrityException {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FileContentsDecryptionException {
    #[error("Failed to decrypt file contents: {0}")]
    WithCause(#[from] anyhow::Error),
    #[error("Failed to decrypt file contents")]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeMetadataPart {
    Key = 0,
    Passphrase = 1,
    Name = 2,
    ExtendedAttributes = 3,
    ContentKey = 4,
    HashKey = 5,
    BlockSignature = 6,
    Thumbnail = 7,
}

#[derive(Debug, thiserror::Error)]
pub enum NodeMetadataDecryptionException {
    #[error("Failed to decrypt node metadata: {part:?}: {source}")]
    WithPart {
        part: NodeMetadataPart,
        #[source]
        source: anyhow::Error,
    },
    #[error("Failed to decrypt node metadata: {0}")]
    General(String),
}

pub struct DownloadState {
    pub uid: RevisionUid,
    pub revision_dto: BlockListingRevisionDto,
    pub node_key: PgpPrivateKey,
    pub content_key: PgpSessionKey,
    downloaded_block_digests: Mutex<Vec<Vec<u8>>>,
    bytes_written: AtomicI64,
    is_completed: std::sync::atomic::AtomicBool,
}

impl DownloadState {
    pub fn new(
        uid: RevisionUid,
        revision_dto: BlockListingRevisionDto,
        node_key: PgpPrivateKey,
        content_key: PgpSessionKey,
    ) -> Self {
        Self {
            uid,
            revision_dto,
            node_key,
            content_key,
            downloaded_block_digests: Mutex::new(Vec::new()),
            bytes_written: AtomicI64::new(0),
            is_completed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn get_next_block_index_to_download(&self) -> i32 {
        self.downloaded_block_digests.lock().unwrap().len() as i32 + 1
    }

    pub fn get_downloaded_block_digests(&self) -> Vec<Vec<u8>> {
        self.downloaded_block_digests.lock().unwrap().clone()
    }

    pub fn add_downloaded_block_digest(&self, digest: Vec<u8>) {
        self.downloaded_block_digests.lock().unwrap().push(digest);
    }

    pub fn get_number_of_bytes_written(&self) -> i64 {
        self.bytes_written.load(Ordering::Relaxed)
    }

    pub fn add_number_of_bytes_written(&self, bytes: i64) {
        self.bytes_written.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn set_is_completed(&self) {
        self.is_completed.store(true, Ordering::Relaxed);
        debug!(
            "Download disposed before completion for revision {:?}",
            self.uid
        );
    }
}

pub struct BlockDownloader {
    client: Arc<ProtonDriveClient>,
    pub queue: TransferQueue,
}

impl BlockDownloader {
    pub fn new(client: Arc<ProtonDriveClient>, max_degree_of_parallelism: usize) -> Self {
        Self {
            client,
            queue: TransferQueue::new(max_degree_of_parallelism),
        }
    }

    pub async fn download(
        &self,
        revision_uid: &RevisionUid,
        index: i32,
        bare_url: &str,
        token: &str,
        content_key: &PgpSessionKey,
        output: &mut Vec<u8>,
    ) -> anyhow::Result<Vec<u8>> {
        let max_retries = 4u32;
        let mut last_err: Option<anyhow::Error> = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = Self::retry_delay(attempt);

                // Handle 429 retry-after if applicable
                if let Some(e) = &last_err {
                    if let Some(retry_after) = Self::extract_retry_after(e) {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap();
                        if retry_after > now {
                            let wait = retry_after - now;
                            info!(
                                "Waiting {:?} before retrying blob download due to 429 response",
                                wait
                            );
                            tokio::time::sleep(wait).await;
                        }
                    }
                }

                info!(
                    "Retrying blob download for block #{} of revision {:?} (retry number: {}). Previous attempt error: {:?}",
                    index, revision_uid, attempt, last_err
                );

                output.clear();
                tokio::time::sleep(delay).await;
            }

            match self
                .execute_download(bare_url, token, content_key, output)
                .await
            {
                Ok(digest) => return Ok(digest),
                Err(e) => {
                    // Don't retry decryption errors
                    if e.downcast_ref::<FileContentsDecryptionException>()
                        .is_some()
                    {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("Download failed after {} attempts", max_retries + 1)
        }))
    }

    async fn execute_download(
        &self,
        bare_url: &str,
        token: &str,
        content_key: &PgpSessionKey,
        output: &mut Vec<u8>,
    ) -> anyhow::Result<Vec<u8>> {
        let response: reqwest::Response = self
            .client
            .api()
            .storage()
            .get_blob_stream(bare_url, token)
            .await?;
        let blob_bytes = response.bytes().await?;

        let mut hasher = Sha256::new();
        hasher.update(&blob_bytes);

        let alg = SymmetricKeyAlgorithm::from(content_key.algorithm);
        let sk = SessionKey::new(&content_key.key, alg);

        let result = Decryptor::default()
            .with_session_key(sk)
            .decrypt(&blob_bytes, DataEncoding::Auto)?;

        output.extend_from_slice(&result.data);

        Ok(hasher.finalize().to_vec())
    }

    fn retry_delay(attempt: u32) -> Duration {
        Duration::from_millis(500 * (1u64 << (attempt - 1).min(4)))
    }

    fn extract_retry_after(_err: &anyhow::Error) -> Option<Duration> {
        None
    }
}

pub trait FileDownloader: Send + Sync {
    fn download_to_stream(
        &self,
        content_output_stream: Box<dyn Write + Send>,
        on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    ) -> DownloadController;

    fn download_to_file(
        &self,
        file_path: &str,
        on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    ) -> DownloadController;
}

pub struct DownloadController {
    state_tx: watch::Sender<ControllerState>,
    pub completion: tokio::task::JoinHandle<anyhow::Result<()>>,
    is_download_complete_with_verification_issue: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerState {
    Running,
    Paused,
}

impl DownloadController {
    pub fn new(
        state_tx: watch::Sender<ControllerState>,
        completion: tokio::task::JoinHandle<anyhow::Result<()>>,
    ) -> Self {
        Self {
            state_tx,
            completion,
            is_download_complete_with_verification_issue: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn is_paused(&self) -> bool {
        *self.state_tx.borrow() == ControllerState::Paused
    }

    pub fn pause(&self) {
        let _ = self.state_tx.send(ControllerState::Paused);
    }

    pub fn resume(&self) {
        let _ = self.state_tx.send(ControllerState::Running);
    }

    pub fn get_is_download_complete_with_verification_issue(&self) -> bool {
        self.is_download_complete_with_verification_issue
            .load(Ordering::Relaxed)
    }
}

pub struct RevisionReader {
    client: Arc<ProtonDriveClient>,
    state: Arc<DownloadState>,
    release_block_listing: Box<dyn Fn(i32) + Send + Sync>,
}

impl RevisionReader {
    pub fn new(
        client: Arc<ProtonDriveClient>,
        state: Arc<DownloadState>,
        release_block_listing: Box<dyn Fn(i32) + Send + Sync>,
    ) -> Self {
        Self {
            client,
            state,
            release_block_listing,
        }
    }

    pub async fn read_next_block(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        let block_index = self.state.get_next_block_index_to_download();
        let total_blocks = self.state.revision_dto.blocks.len() as i32;

        if block_index > total_blocks {
            return Ok(None);
        }

        let block_dto = &self.state.revision_dto.blocks[(block_index - 1) as usize];

        let mut block_data = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut block_data);

        let digest = self
            .client
            .block_downloader()
            .download(
                &self.client,
                self.state.uid.clone(),
                block_index,
                block_dto.bare_url.clone(),
                block_dto.token.clone(),
                self.state.content_key.clone(),
                &mut cursor,
            )
            .await?;

        self.state.add_downloaded_block_digest(digest);
        self.state
            .add_number_of_bytes_written(block_data.len() as i64);

        (self.release_block_listing)(1);

        Ok(Some(block_data))
    }
}
