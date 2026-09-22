use crate::client::ProtonDriveClient;
use crate::meta::AdditionalMetadataProperty;
use crate::node::download::ControllerState;
use crate::node::draft::RevisionDraftProvider;
use crate::node::revision::RevisionOperations;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Instant;
use tokio::sync::watch;

#[derive(serde::Serialize)]
struct UploadPerformanceTelemetry {
    route: &'static str,
    file_size_class: &'static str,
    block_count_class: &'static str,
    total_time_ms: u128,
    active_time_ms: u128,
    throughput_kib_per_second: u64,
    validation_outcome: &'static str,
}

fn upload_classes(size: i64, blocks: i32) -> (&'static str, &'static str) {
    let size_class = if blocks > 1 {
        "multi"
    } else if size < 128 * 1024 {
        "small"
    } else {
        "single"
    };
    let block_class = if blocks > 4 {
        "many"
    } else if blocks > 1 {
        "few"
    } else {
        "single"
    };
    (size_class, block_class)
}

pub struct UploadController {
    state_tx: watch::Sender<ControllerState>,
    sdk_events: Arc<crate::events::SdkEvents>,
}

impl UploadController {
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
}

pub struct FileUploader {
    client: Arc<ProtonDriveClient>,
    revision_draft_provider: Box<dyn RevisionDraftProvider>,
    remaining_number_of_blocks: AtomicI32,
    size: i64,
    last_modification_time: Option<DateTime<Utc>>,
    additional_metadata: Option<Vec<AdditionalMetadataProperty>>,
    media_info: Option<crate::api::attr::MediaExtendedAttributes>,
    expected_sha1: Option<Vec<u8>>,
    state_tx: watch::Sender<ControllerState>,
    request_started: Instant,
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
        let request_started = Instant::now();
        let expected_number_of_blocks = (size + 4 * 1024 * 1024 - 1) / (4 * 1024 * 1024);
        client
            .revision_creation_semaphore()
            .acquire(expected_number_of_blocks as usize)
            .await?;

        let (state_tx, _) = watch::channel(ControllerState::Running);
        Ok(Self {
            client: Arc::new(client.clone()),
            revision_draft_provider,
            remaining_number_of_blocks: AtomicI32::new(expected_number_of_blocks as i32),
            size,
            last_modification_time: last_modification_time.map(DateTime::from),
            additional_metadata,
            media_info,
            expected_sha1: None,
            state_tx,
            request_started,
        })
    }

    pub fn controller(&self) -> UploadController {
        UploadController {
            state_tx: self.state_tx.clone(),
            sdk_events: self.client.sdk_events().clone(),
        }
    }

    /// When set, commit fails with [`ChecksumMismatchIntegrityException`] if the uploaded content SHA-1 differs.
    pub fn set_expected_sha1(&mut self, sha1: Vec<u8>) {
        self.expected_sha1 = Some(sha1);
    }

    pub async fn upload_from_stream(
        &self,
        content_stream: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        thumbnails: Vec<crate::node::thumbnail::Thumbnail>,
        on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    ) -> anyhow::Result<crate::node::NodeUid> {
        let active_started = Instant::now();
        let result = self
            .upload_from_stream_inner(content_stream, thumbnails, on_progress)
            .await;
        let active_time = active_started.elapsed();
        let blocks = ((self.size + 4 * 1024 * 1024 - 1) / (4 * 1024 * 1024)) as i32;
        let (file_size_class, block_count_class) = upload_classes(self.size, blocks);
        let validation_outcome = match (&result, self.expected_sha1.is_some()) {
            (Ok(_), true) => "verified",
            (Ok(_), false) => "not_requested",
            (Err(error), _)
                if error
                    .downcast_ref::<crate::error::ChecksumMismatchIntegrityException>()
                    .is_some()
                    || error
                        .downcast_ref::<crate::error::ContentSizeMismatchIntegrityException>()
                        .is_some() =>
            {
                "failed"
            }
            (Err(_), _) => "not_completed",
        };
        let payload = UploadPerformanceTelemetry {
            route: "block",
            file_size_class,
            block_count_class,
            total_time_ms: self.request_started.elapsed().as_millis(),
            active_time_ms: active_time.as_millis(),
            throughput_kib_per_second: if active_time.is_zero() {
                0
            } else {
                (self.size.max(0) as f64 / 1024.0 / active_time.as_secs_f64()) as u64
            },
            validation_outcome,
        };
        if let Ok(payload) = serde_json::to_vec(&payload) {
            self.client
                .telemetry()
                .record_metric("uploadPerformance".into(), Some(payload))
                .await;
        }
        if let Err(e) = &result {
            self.client
                .telemetry()
                .record_metric("uploadError".into(), Some(e.to_string().into_bytes()))
                .await;
        }
        result
    }

    async fn upload_from_stream_inner(
        &self,
        content_stream: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        thumbnails: Vec<crate::node::thumbnail::Thumbnail>,
        on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    ) -> anyhow::Result<crate::node::NodeUid> {
        let draft = self.revision_draft_provider.get_draft().await?;
        let node_uid = draft.uid.node_uid.clone();

        let on_progress_arc: Arc<dyn Fn(i64, i64) + Send + Sync> = Arc::from(on_progress);

        let release_blocks_action = Box::new(|_| {});

        let mut writer = RevisionOperations::open_for_writing(
            &self.client,
            draft,
            release_blocks_action,
            self.size,
            self.last_modification_time,
            self.additional_metadata.clone(),
            self.media_info.clone(),
            self.expected_sha1.clone(),
        )
        .await?;

        writer.upload_thumbnails(thumbnails).await?;

        writer
            .write(
                content_stream,
                on_progress_arc,
                Some(self.state_tx.subscribe()),
            )
            .await?;
        writer.commit().await?;
        Ok(node_uid)
    }
}

#[cfg(test)]
mod tests {
    use super::upload_classes;

    #[test]
    fn upload_classification_matches_route_buckets() {
        assert_eq!(upload_classes(127 * 1024, 1), ("small", "single"));
        assert_eq!(upload_classes(128 * 1024, 1), ("single", "single"));
        assert_eq!(upload_classes(8 * 1024 * 1024, 2), ("multi", "few"));
        assert_eq!(upload_classes(24 * 1024 * 1024, 6), ("multi", "many"));
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
