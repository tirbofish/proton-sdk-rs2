use crate::client::ProtonDriveClient;
use crate::meta::AdditionalMetadataProperty;
use crate::node::draft::RevisionDraftProvider;
use crate::node::revision::RevisionOperations;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

pub struct FileUploader {
    client: Arc<ProtonDriveClient>,
    revision_draft_provider: Box<dyn RevisionDraftProvider>,
    size: i64,
    remaining_number_of_blocks: AtomicI32,
}

impl FileUploader {
    pub async fn create(
        client: &ProtonDriveClient,
        revision_draft_provider: Box<dyn RevisionDraftProvider>,
        size: i64,
        _last_modification_time: Option<std::time::SystemTime>,
        _additional_metadata: Option<Vec<AdditionalMetadataProperty>>,
    ) -> anyhow::Result<Self> {
        let expected_number_of_blocks = (size + 4 * 1024 * 1024 - 1) / (4 * 1024 * 1024);
        client
            .revision_creation_semaphore()
            .acquire(expected_number_of_blocks as usize)
            .await?;

        Ok(Self {
            client: Arc::new(client.clone()),
            revision_draft_provider,
            size,
            remaining_number_of_blocks: AtomicI32::new(expected_number_of_blocks as i32),
        })
    }

    pub async fn upload_from_stream(
        &self,
        _content_stream: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        _on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    ) -> anyhow::Result<()> {
        let draft = self.revision_draft_provider.get_draft().await?;
        let _writer =
            RevisionOperations::open_for_writing(&self.client, draft, Box::new(|_| {})).await?;

        // Simplified upload logic
        Ok(())
    }
}

impl Drop for FileUploader {
    fn drop(&mut self) {
        let remaining = self.remaining_number_of_blocks.swap(0, Ordering::SeqCst);
        if remaining > 0 {
            self.client
                .revision_creation_semaphore()
                .release(remaining as usize);
        }
    }
}
