use crate::api::block::BlockListingRevisionDto;
use crate::client::ProtonDriveClient;
use crate::node::revision::RevisionUid;
use crate::node::transfer::TransferQueue;
use crate::pgp::{PgpPrivateKey, PgpSessionKey};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use log::{debug, error, info};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::time::Duration;
use tokio::sync::watch;

pub const MIN_BLOCK_INDEX: i32 = 1;
pub const DEFAULT_BLOCK_PAGE_SIZE: i32 = 10;

pub const MAX_CONCURRENT_DOWNLOADS: usize = 5;
pub const MAX_BUFFERED_DOWNLOAD_BLOCKS: usize = 10;

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
        let _permit = self.queue.start_block().await?;
        let max_retries = 4u32;
        let mut last_err: Option<anyhow::Error> = None;
        let mut was_throttled = false;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = Self::retry_delay(attempt);

                if let Some(e) = &last_err {
                    if let Some(retry_after) = Self::extract_retry_after(e) {
                        info!(
                            "Waiting {:?} before retrying blob download due to 429 response",
                            retry_after
                        );
                        self.client.sdk_events().requests_throttled();
                        was_throttled = true;
                        tokio::time::sleep(retry_after).await;
                    } else {
                        tokio::time::sleep(delay).await;
                    }
                }

                info!(
                    "Retrying blob download for block #{} of revision {:?} (retry number: {}). Previous attempt error: {:?}",
                    index, revision_uid, attempt, last_err
                );

                output.clear();
            }

            match self
                .execute_download(bare_url, token, content_key, output)
                .await
            {
                Ok(digest) => {
                    if was_throttled {
                        self.client.sdk_events().requests_unthrottled();
                    }
                    return Ok(digest);
                }
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

        debug!("Downloaded {} encrypted bytes", blob_bytes.len());

        let mut hasher = Sha256::new();
        hasher.update(&blob_bytes);

        let data = match content_key.decrypt(&blob_bytes) {
            Ok(data) => data,
            Err(e) => {
                error!("Failed to decrypt block: {:?}", e);
                return Err(FileContentsDecryptionException::WithCause(e).into());
            }
        };

        debug!("Decrypted to {} bytes", data.len());
        output.extend_from_slice(&data);

        Ok(hasher.finalize().to_vec())
    }

    fn retry_delay(attempt: u32) -> Duration {
        crate::error::retry_backoff_delay(attempt)
    }

    fn extract_retry_after(err: &anyhow::Error) -> Option<Duration> {
        err.downcast_ref::<crate::error::TooManyRequestsException>()
            .and_then(|e| e.retry_after)
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
    sdk_events: Arc<crate::events::SdkEvents>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerState {
    Running,
    Paused,
}

pub(crate) async fn wait_while_paused(pause_rx: &mut Option<watch::Receiver<ControllerState>>) {
    let Some(rx) = pause_rx.as_mut() else {
        return;
    };
    while *rx.borrow() == ControllerState::Paused {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

impl DownloadController {
    pub fn new(
        state_tx: watch::Sender<ControllerState>,
        completion: tokio::task::JoinHandle<anyhow::Result<()>>,
        sdk_events: Arc<crate::events::SdkEvents>,
    ) -> Self {
        Self {
            state_tx,
            completion,
            is_download_complete_with_verification_issue: std::sync::atomic::AtomicBool::new(false),
            sdk_events,
        }
    }

    pub fn is_paused(&self) -> bool {
        *self.state_tx.borrow() == ControllerState::Paused
    }

    pub fn pause(&self) {
        self.sdk_events.transfers_paused();
        let _ = self.state_tx.send(ControllerState::Paused);
    }

    pub fn resume(&self) {
        self.sdk_events.transfers_resumed();
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

    /// Download all blocks in parallel and write them to the output stream in order.
    /// This is significantly faster than sequential downloads for files with many blocks.
    ///
    /// The method downloads up to `MAX_CONCURRENT_DOWNLOADS` blocks simultaneously,
    /// buffers them, and writes them to the output stream in the correct order.
    pub async fn read_all_blocks_parallel<W: std::io::Write + Send + ?Sized>(
        &self,
        output: &mut W,
        on_progress: impl Fn(i64, i64) + Send + Sync,
        mut pause_rx: Option<watch::Receiver<ControllerState>>,
    ) -> anyhow::Result<()> {
        let total_blocks = self.state.revision_dto.blocks.len();
        if total_blocks == 0 {
            return Ok(());
        }

        let total_size = self.state.revision_dto.revision.size;
        let content_key = self.state.content_key.clone();

        // Track which block we're downloading next and which we're writing next
        let next_download_index = Arc::new(AtomicI32::new(1));
        let mut next_write_index: i32 = 1;
        let mut bytes_written: i64 = 0;

        // Buffer for out-of-order blocks awaiting write
        let mut buffered_blocks: BTreeMap<i32, (Vec<u8>, Vec<u8>)> = BTreeMap::new();

        // Active downloads
        let mut download_futures: FuturesUnordered<
            tokio::task::JoinHandle<anyhow::Result<(i32, Vec<u8>, Vec<u8>)>>,
        > = FuturesUnordered::new();

        // Start initial batch of downloads
        wait_while_paused(&mut pause_rx).await;
        while download_futures.len() < MAX_CONCURRENT_DOWNLOADS
            && next_download_index.load(Ordering::SeqCst) <= total_blocks as i32
        {
            let idx = next_download_index.fetch_add(1, Ordering::SeqCst);
            if idx > total_blocks as i32 {
                break;
            }

            let block_dto = &self.state.revision_dto.blocks[(idx - 1) as usize];
            let client = self.client.clone();
            let bare_url = block_dto.bare_url.clone();
            let token = block_dto.token.clone();
            let ck = content_key.clone();

            let handle = tokio::spawn(async move {
                Self::download_and_decrypt_block(&client, idx, &bare_url, &token, &ck).await
            });
            download_futures.push(handle);
        }

        // Process downloads as they complete
        while !download_futures.is_empty() || next_write_index <= total_blocks as i32 {
            // Wait for a download to complete
            if let Some(result) = download_futures.next().await {
                let (block_idx, data, digest) = result??;

                debug!(
                    "Block {} downloaded ({} bytes), next_write={}",
                    block_idx,
                    data.len(),
                    next_write_index
                );

                // Start a new download if there are more blocks
                wait_while_paused(&mut pause_rx).await;
                let next_idx = next_download_index.fetch_add(1, Ordering::SeqCst);
                if next_idx <= total_blocks as i32 {
                    let block_dto = &self.state.revision_dto.blocks[(next_idx - 1) as usize];
                    let client = self.client.clone();
                    let bare_url = block_dto.bare_url.clone();
                    let token = block_dto.token.clone();
                    let ck = content_key.clone();

                    let handle = tokio::spawn(async move {
                        Self::download_and_decrypt_block(&client, next_idx, &bare_url, &token, &ck)
                            .await
                    });
                    download_futures.push(handle);
                }

                // Buffer this block
                buffered_blocks.insert(block_idx, (data, digest));

                // Write any blocks we can in order
                while let Some((data, digest)) = buffered_blocks.remove(&next_write_index) {
                    output.write_all(&data)?;
                    bytes_written += data.len() as i64;

                    self.state.add_downloaded_block_digest(digest);
                    self.state.add_number_of_bytes_written(data.len() as i64);
                    (self.release_block_listing)(1);

                    on_progress(bytes_written, total_size);
                    next_write_index += 1;
                }
            } else if download_futures.is_empty() {
                // No more pending downloads, write remaining buffered blocks
                while let Some((idx, (data, digest))) = buffered_blocks.pop_first() {
                    if idx != next_write_index {
                        return Err(anyhow::anyhow!(
                            "Block ordering error: expected {}, got {}",
                            next_write_index,
                            idx
                        ));
                    }
                    output.write_all(&data)?;
                    bytes_written += data.len() as i64;

                    self.state.add_downloaded_block_digest(digest);
                    self.state.add_number_of_bytes_written(data.len() as i64);
                    (self.release_block_listing)(1);

                    on_progress(bytes_written, total_size);
                    next_write_index += 1;
                }
                break;
            }
        }

        debug!(
            "Parallel download complete: {} blocks, {} bytes",
            total_blocks, bytes_written
        );

        Ok(())
    }

    pub async fn download_block_at(&self, block_index: i32) -> anyhow::Result<Vec<u8>> {
        let total = self.state.revision_dto.blocks.len() as i32;
        if block_index < 1 || block_index > total {
            anyhow::bail!("block index {block_index} out of range");
        }
        let block_dto = &self.state.revision_dto.blocks[(block_index - 1) as usize];
        let (_, data, _) = Self::download_and_decrypt_block(
            &self.client,
            block_index,
            &block_dto.bare_url,
            &block_dto.token,
            &self.state.content_key,
        )
        .await?;
        Ok(data)
    }

    /// Download and decrypt a single block, returning (block_index, decrypted_data, sha256_digest).
    async fn download_and_decrypt_block(
        client: &ProtonDriveClient,
        block_index: i32,
        bare_url: &str,
        token: &str,
        content_key: &PgpSessionKey,
    ) -> anyhow::Result<(i32, Vec<u8>, Vec<u8>)> {
        let _permit = client.block_downloader().queue.start_block().await?;
        let response = client
            .api()
            .storage()
            .get_blob_stream(bare_url, token)
            .await?;
        let blob_bytes = response.bytes().await?;

        // Hash the encrypted bytes for verification
        let mut hasher = Sha256::new();
        hasher.update(&blob_bytes);
        let digest = hasher.finalize().to_vec();

        let data = match content_key.decrypt(&blob_bytes) {
            Ok(data) => data,
            Err(e) => {
                error!("Failed to decrypt block {}: {:?}", block_index, e);
                client
                    .telemetry()
                    .record_metric("decryptionError".into(), Some(e.to_string().into_bytes()))
                    .await;
                return Err(FileContentsDecryptionException::WithCause(e).into());
            }
        };

        debug!(
            "Block {} decrypted: {} encrypted -> {} decrypted bytes",
            block_index,
            blob_bytes.len(),
            data.len()
        );

        Ok((block_index, data, digest))
    }
}
