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

        let (state_tx, _) = tokio::sync::watch::channel(ControllerState::Running);

        let completion = tokio::spawn(async move {
            let release_block_listing = Box::new(|_| {});
            tracing::debug!("Creating download state for revision_uid={}", revision_uid);
            let download_state = RevisionOperations::create_download_state(
                &client,
                revision_uid.clone(),
                release_block_listing,
            )
            .await
            .map_err(|e| {
                tracing::error!("Failed to create download state for {}: {:?}", revision_uid, e);
                e
            })?;

            let num_blocks = download_state.revision_dto.blocks.len();
            tracing::debug!("Download state created: {} blocks, size={}", num_blocks, download_state.revision_dto.revision.size);

            let download_state = Arc::new(download_state);
            let reader = RevisionOperations::open_for_reading(
                &client,
                download_state.clone(),
                Box::new(|_| {}),
            );

            // Use parallel downloads for better performance (downloads up to 5 blocks concurrently)
            reader
                .read_all_blocks_parallel(&mut *content_output_stream, |downloaded, total| {
                    on_progress(downloaded, total);
                })
                .await?;
            
            // Ensure all buffered data is flushed before returning
            content_output_stream.flush()?;
            
            Ok(())
        });

        DownloadController::new(state_tx, completion)
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
