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
        _content_output_stream: Box<dyn std::io::Write + Send>,
        _on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    ) -> DownloadController {
        let client = self.client.clone();
        let revision_uid = self.revision_uid.clone();

        let (state_tx, _) = tokio::sync::watch::channel(ControllerState::Running);

        let completion = tokio::spawn(async move {
            let release_block_listing = Box::new(|_| {}); // Simplified
            let _state = RevisionOperations::create_download_state(
                &client,
                revision_uid,
                release_block_listing,
            )
            .await?;

            // Simplified reading logic
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
