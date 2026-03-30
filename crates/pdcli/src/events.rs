use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::NodeUid;
use proton_drive_sdk::volume::VolumeId;
use std::sync::Arc;

use crate::index::NodeIndex;

/// Drive event types defined by the Proton Drive API.
const EVENT_TYPE_DELETE: i32 = 1;

/// Spawns the event watcher using the correct two-level polling architecture:
///
/// 1. A **core event loop** polls `/core/v5/events/{id}` every 30 seconds.
///    The core response's `DriveShareRefresh` field signals that volume-level
///    changes have occurred and volume events should be fetched.
///
/// 2. When `drive_share_refresh` is set, a **volume event drain** fetches all
///    pending volume-event pages (following `more == true`) and applies them
///    to the local index.
///
/// This matches the Proton API contract: volume events are only triggered by
/// the core loop, not polled independently.
pub fn spawn_event_watcher(
    drive: Arc<ProtonDriveClient>,
    index: Arc<NodeIndex>,
    volume_id: VolumeId,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Obtain the starting cursors for both event streams.
        let mut core_event_id = match drive.get_core_latest_event_id().await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Failed to get initial core event ID: {e}");
                return;
            }
        };
        let mut volume_event_id = match drive.get_volume_latest_event_id(volume_id.clone()).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Failed to get initial volume event ID: {e}");
                return;
            }
        };

        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

            // ── Core event loop ──────────────────────────────────────────────
            let core_resp = match drive.poll_core_events(&core_event_id).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Core event poll failed: {e}");
                    continue;
                }
            };

            core_event_id = core_resp.event_id.clone();

            // If the server asked for a full refresh of core data, flush everything.
            if core_resp.refresh != 0 {
                tracing::info!("Core refresh requested — clearing all indexed markers");
                index.unmark_all_indexed();
            }

            // ── Volume events (triggered by DriveShareRefresh) ───────────────
            // Any non-None DriveShareRefresh means Drive content may have changed.
            if core_resp.drive_share_refresh.is_some() {
                volume_event_id = drain_volume_events(
                    &drive,
                    &index,
                    &volume_id,
                    volume_event_id,
                )
                .await;
            }

            // If core has more pages, loop immediately (no sleep) to drain them.
            // We do this after volume events so the index stays consistent.
            if core_resp.more != 0 {
                // Re-poll without sleeping — overwrite event_id at loop top.
                continue;
            }
        }
    })
}

/// Fetches and applies all pending volume event pages starting from `from_id`.
/// Returns the new cursor to use for the next call.
async fn drain_volume_events(
    drive: &ProtonDriveClient,
    index: &NodeIndex,
    volume_id: &VolumeId,
    mut event_id: String,
) -> String {
    loop {
        let resp = match drive.poll_volume_events(volume_id.clone(), &event_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Volume event poll failed: {e}");
                break;
            }
        };

        event_id = resp.event_id.clone();

        if resp.refresh {
            tracing::info!("Volume refresh requested by server — clearing indexed markers");
            index.unmark_all_indexed();
        }

        for event in &resp.events {
            let uid = NodeUid::new(volume_id.clone(), event.link.link_id.clone());

            if event.event_type == EVENT_TYPE_DELETE || event.link.is_trashed {
                index.remove(&uid);
            } else {
                // Create/update: unmark the parent folder so the next `ls` re-fetches it,
                // picking up the new or changed node with correct metadata.
                let parent_uid = event.link.parent_link_id.as_ref()
                    .map(|pid| NodeUid::new(volume_id.clone(), pid.clone()));
                if let Some(p) = &parent_uid {
                    index.unmark_indexed(p);
                }
                // Also refresh the node itself if it's already in the index.
                index.unmark_indexed(&uid);
            }
        }

        if !resp.more {
            break;
        }
    }

    event_id
}
