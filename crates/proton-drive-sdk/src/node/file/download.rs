use crate::client::ProtonDriveClient;
use crate::node::download::{ControllerState, DownloadController};
use crate::node::revision::{RevisionOperations, RevisionUid};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

pub struct FileDownloader {
    client: Arc<ProtonDriveClient>,
    revision_uid: RevisionUid,
    remaining_number_of_blocks_to_list: AtomicI32,
}

impl FileDownloader {
    pub async fn create(
        client: &ProtonDriveClient,
        revision_uid: RevisionUid,
    ) -> anyhow::Result<Self> {
        client.block_listing_semaphore().acquire(1).await?;

        Ok(Self {
            client: Arc::new(client.clone()),
            revision_uid,
            remaining_number_of_blocks_to_list: AtomicI32::new(1),
        })
    }

    pub fn download_to_stream(
        &self,
        mut content_output_stream: Box<dyn std::io::Write + Send>,
        on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    ) -> DownloadController {
        let client = self.client.clone();
        let revision_uid = self.revision_uid.clone();

        let (state_tx, state_rx) = tokio::sync::watch::channel(ControllerState::Running);

        let completion = tokio::spawn(async move {
            let _file_permit = client.block_downloader().queue.start_file().await?;
            let release_block_listing = Box::new(|_| {});
            tracing::debug!("Creating download state for revision_uid={}", revision_uid);
            let download_state = RevisionOperations::create_download_state(
                &client,
                revision_uid.clone(),
                release_block_listing,
            )
            .await
            .map_err(|e| {
                tracing::error!(
                    "Failed to create download state for {}: {:?}",
                    revision_uid,
                    e
                );
                e
            })?;

            let num_blocks = download_state.revision_dto.blocks.len();
            tracing::debug!(
                "Download state created: {} blocks, size={}",
                num_blocks,
                download_state.revision_dto.revision.size
            );

            let download_state = Arc::new(download_state);
            let reader = RevisionOperations::open_for_reading(
                &client,
                download_state.clone(),
                Box::new(|_| {}),
            );

            // Use parallel downloads for better performance (downloads up to 5 blocks concurrently)
            let result = reader
                .read_all_blocks_parallel(
                    &mut *content_output_stream,
                    |downloaded, total| {
                        on_progress(downloaded, total);
                    },
                    Some(state_rx),
                )
                .await;

            if let Err(e) = &result {
                client
                    .telemetry()
                    .record_metric("downloadError".into(), Some(e.to_string().into_bytes()))
                    .await;
            }
            result?;

            // Ensure all buffered data is flushed before returning
            content_output_stream.flush()?;

            Ok(())
        });

        DownloadController::new(state_tx, completion, self.client.sdk_events().clone())
    }

    /// Claimed plaintext sizes of each block, required for seeking. Fails on older files that omit them.
    pub async fn claimed_block_sizes(&self) -> anyhow::Result<Vec<i32>> {
        use crate::api::attr::ExtendedAttributes;
        use crate::author::Author;
        use crate::node::authorship::AuthorshipClaim;
        use crate::node::crypto::NodeCrypto;
        use crate::node::file::FileOperations;

        let secrets =
            FileOperations::get_secrets(&self.client, self.revision_uid.node_uid.clone()).await?;
        let resp = self
            .client
            .api()
            .files()
            .get_revisions(
                self.revision_uid.node_uid.volume_id.clone(),
                self.revision_uid.node_uid.link_id.clone(),
            )
            .await?;
        let dto = resp
            .revisions
            .into_iter()
            .find(|r| r.id == self.revision_uid.revision_id)
            .ok_or_else(|| anyhow::anyhow!("revision not found"))?;
        let xattr_msg = dto
            .extended_attributes
            .ok_or_else(|| anyhow::anyhow!("revision has no claimed block sizes"))?;
        let claim = AuthorshipClaim {
            keys: vec![],
            author: Author::ANONYMOUS,
            key_retrieval_error_message: None,
        };
        let (bytes, _, _) =
            NodeCrypto::decrypt_message(&xattr_msg, None, [&secrets.base.key], &claim)
                .map_err(|e| anyhow::anyhow!(e))?;
        let xattr: ExtendedAttributes = serde_json::from_slice(&bytes)?;
        xattr
            .common
            .and_then(|c| c.block_sizes)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("revision has no claimed block sizes"))
    }

    /// Downloads `[offset, offset+length)` of plaintext using claimed block sizes.
    pub async fn download_range(
        &self,
        offset: u64,
        length: u64,
        output: &mut dyn std::io::Write,
    ) -> anyhow::Result<u64> {
        let sizes = self.claimed_block_sizes().await?;
        let download_state = Arc::new(
            crate::node::revision::RevisionOperations::create_download_state(
                &self.client,
                self.revision_uid.clone(),
                Box::new(|_| {}),
            )
            .await?,
        );
        let reader = crate::node::revision::RevisionOperations::open_for_reading(
            &self.client,
            download_state,
            Box::new(|_| {}),
        );
        let mut cursor = 0u64;
        let end = offset.saturating_add(length);
        let mut written = 0u64;
        for (i, size) in sizes.iter().enumerate() {
            let block_start = cursor;
            let block_end = cursor + *size as u64;
            cursor = block_end;
            if block_end <= offset {
                continue;
            }
            if block_start >= end {
                break;
            }
            let data = reader.download_block_at((i as i32) + 1).await?;
            let slice_start = offset.saturating_sub(block_start) as usize;
            let slice_end = (end.min(block_end) - block_start) as usize;
            output.write_all(&data[slice_start..slice_end.min(data.len())])?;
            written += (slice_end - slice_start) as u64;
        }
        Ok(written)
    }

    /// Creates a cursor for on-demand range reads, suitable for media playback.
    pub async fn get_seekable_stream(&self) -> anyhow::Result<SeekableFileStream<'_>> {
        let length =
            self.claimed_block_sizes()
                .await?
                .into_iter()
                .try_fold(0u64, |total, size| {
                    u64::try_from(size)
                        .ok()
                        .and_then(|size| total.checked_add(size))
                        .ok_or_else(|| anyhow::anyhow!("invalid claimed block sizes"))
                })?;
        Ok(SeekableFileStream {
            downloader: self,
            position: 0,
            length,
        })
    }
}

pub struct SeekableFileStream<'a> {
    downloader: &'a FileDownloader,
    position: u64,
    length: u64,
}

impl SeekableFileStream<'_> {
    pub fn position(&self) -> u64 {
        self.position
    }

    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn seek(&mut self, position: u64) -> anyhow::Result<()> {
        if position > self.length {
            anyhow::bail!(
                "seek position {position} exceeds file length {}",
                self.length
            );
        }
        self.position = position;
        Ok(())
    }

    pub async fn read(&mut self, num_bytes: usize) -> anyhow::Result<Vec<u8>> {
        if num_bytes == 0 {
            anyhow::bail!("read length must be greater than zero");
        }
        let mut bytes = Vec::with_capacity(num_bytes.min((self.length - self.position) as usize));
        let written = self
            .downloader
            .download_range(self.position, num_bytes as u64, &mut bytes)
            .await?;
        self.position += written;
        Ok(bytes)
    }
}

impl Drop for FileDownloader {
    fn drop(&mut self) {
        let remaining = self
            .remaining_number_of_blocks_to_list
            .swap(0, Ordering::SeqCst);
        if remaining > 0 {
            self.client
                .block_listing_semaphore()
                .release(remaining as usize);
        }
    }
}
