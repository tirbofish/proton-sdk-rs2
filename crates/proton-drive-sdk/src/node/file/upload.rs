use crate::client::ProtonDriveClient;
use crate::meta::AdditionalMetadataProperty;
use crate::node::draft::RevisionDraftProvider;
use crate::node::revision::RevisionOperations;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

pub struct FileUploader {
    client: Arc<ProtonDriveClient>,
    revision_draft_provider: Box<dyn RevisionDraftProvider>,
    remaining_number_of_blocks: AtomicI32,
    size: i64,
    last_modification_time: Option<DateTime<Utc>>,
    additional_metadata: Option<Vec<AdditionalMetadataProperty>>,
    media_info: Option<crate::api::attr::MediaExtendedAttributes>,
}

impl FileUploader {
    pub async fn create(
        client: &ProtonDriveClient,
        revision_draft_provider: Box<dyn RevisionDraftProvider>,
        size: i64,
        last_modification_time: Option<std::time::SystemTime>,
        additional_metadata: Option<Vec<AdditionalMetadataProperty>>,
        media_info: Option<crate::api::attr::MediaExtendedAttributes>,
    ) -> anyhow::Result<Self> {
        let expected_number_of_blocks = (size + 4 * 1024 * 1024 - 1) / (4 * 1024 * 1024);
        client
            .revision_creation_semaphore()
            .acquire(expected_number_of_blocks as usize)
            .await?;

        Ok(Self {
            client: Arc::new(client.clone()),
            revision_draft_provider,
            remaining_number_of_blocks: AtomicI32::new(expected_number_of_blocks as i32),
            size,
            last_modification_time: last_modification_time.map(DateTime::from),
            additional_metadata,
            media_info,
        })
    }

    pub async fn upload_from_stream(
        &self,
        content_stream: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        thumbnails: Vec<crate::node::thumbnail::Thumbnail>,
        on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    ) -> anyhow::Result<crate::node::NodeUid> {
        let draft = self.revision_draft_provider.get_draft().await?;
        let node_uid = draft.uid.node_uid.clone();

        let on_progress_arc: Arc<dyn Fn(i64, i64) + Send + Sync> = Arc::from(on_progress);

        let release_blocks_action = Box::new(|_| {
            // Sequential implementation for now
        });

        let mut writer =
            RevisionOperations::open_for_writing(
                &self.client,
                draft,
                release_blocks_action,
                self.size,
                self.last_modification_time,
                self.additional_metadata.clone(),
                self.media_info.clone(),
            )
            .await?;

        writer.upload_thumbnails(thumbnails).await?;

        writer.write(content_stream, on_progress_arc).await?;
        writer.commit().await?;
        Ok(node_uid)
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
