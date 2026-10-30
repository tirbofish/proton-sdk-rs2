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

            let metric_item = crate::telemetry::MetricItem::from_revision(
                &revision_uid.node_uid,
                &download_state.revision_dto.revision,
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
                if e.downcast_ref::<crate::node::download::FileContentsDecryptionException>()
                    .is_some()
                {
                    client
                        .integrity_reporter()
                        .report(
                            &client,
                            &metric_item,
                            "content",
                            Some(&e.to_string()),
                            false,
                            None,
                        )
                        .await;
                }
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
        let metric_item =
            crate::telemetry::MetricItem::from_revision(&self.revision_uid.node_uid, &dto);
        let xattr_msg = dto
            .extended_attributes
            .ok_or_else(|| anyhow::anyhow!("revision has no claimed block sizes"))?;
        let claim = AuthorshipClaim {
            keys: vec![],
            author: Author::ANONYMOUS,
            key_retrieval_error_message: None,
        };
        let (bytes, _, _) = self
            .client
            .integrity_reporter()
            .decrypt_message(
                &self.client,
                &metric_item,
                "nodeExtendedAttributes",
                &xattr_msg,
                None,
                [&secrets.base.key],
                &claim,
            )
            .await
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

    /// Opens the revision for reading individual blocks on demand. Needs claimed
    /// block sizes to map byte offsets to blocks, so it fails on older files
    /// that omit them; callers can fall back to a full download.
    pub async fn open_block_reader(&self) -> anyhow::Result<BlockReader> {
        let sizes = self
            .claimed_block_sizes()
            .await?
            .into_iter()
            .map(|size| u64::try_from(size).map_err(|_| anyhow::anyhow!("negative block size")))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let state = Arc::new(
            RevisionOperations::create_download_state(
                &self.client,
                self.revision_uid.clone(),
                Box::new(|_| {}),
            )
            .await?,
        );
        anyhow::ensure!(
            state.revision_dto.blocks.len() == sizes.len(),
            "revision lists {} blocks but claims {} block sizes",
            state.revision_dto.blocks.len(),
            sizes.len()
        );
        let reader = RevisionOperations::open_for_reading(&self.client, state, Box::new(|_| {}));
        Ok(BlockReader::new(reader, sizes))
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

/// Random access to a revision's plaintext, one block at a time.
pub struct BlockReader {
    reader: crate::node::download::RevisionReader,
    /// Plaintext offset where each block starts, plus the total length at the end.
    starts: Vec<u64>,
}

impl BlockReader {
    fn new(reader: crate::node::download::RevisionReader, sizes: Vec<u64>) -> Self {
        let mut starts = Vec::with_capacity(sizes.len() + 1);
        let mut offset = 0;
        starts.push(offset);
        for size in sizes {
            offset += size;
            starts.push(offset);
        }
        Self { reader, starts }
    }

    pub fn len(&self) -> u64 {
        *self.starts.last().expect("starts always has the total")
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn block_count(&self) -> usize {
        self.starts.len() - 1
    }

    /// Zero-based block index holding `offset`, or `None` past the end.
    pub fn block_at(&self, offset: u64) -> Option<usize> {
        block_index_at(&self.starts, offset)
    }

    /// Plaintext offset where a zero-based block starts.
    pub fn block_start(&self, index: usize) -> u64 {
        self.starts[index]
    }

    /// Downloads and decrypts one zero-based block.
    pub async fn read_block(&self, index: usize) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(index < self.block_count(), "block {index} out of range");
        self.reader.download_block_at(index as i32 + 1).await
    }
}

fn block_index_at(starts: &[u64], offset: u64) -> Option<usize> {
    let total = *starts.last()?;
    if offset >= total {
        return None;
    }
    // The last start not after `offset`.
    Some(starts.partition_point(|start| *start <= offset) - 1)
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

#[cfg(test)]
mod tests {
    use super::block_index_at;

    #[test]
    fn offsets_map_to_the_block_that_holds_them() {
        let starts = [0, 4, 8, 10];
        assert_eq!(block_index_at(&starts, 0), Some(0));
        assert_eq!(block_index_at(&starts, 3), Some(0));
        assert_eq!(block_index_at(&starts, 4), Some(1));
        assert_eq!(block_index_at(&starts, 9), Some(2));
        assert_eq!(block_index_at(&starts, 10), None);
        assert_eq!(block_index_at(&[0], 0), None);
    }
}
