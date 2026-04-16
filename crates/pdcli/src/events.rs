use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::index::DriveIndex;

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const OFFLINE_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// Background loop that polls Proton Drive volume events and feeds them into
/// the index so that FUSE always sees up-to-date metadata (when online).
///
/// When a poll succeeds, the index is marked online (which also wakes the
/// request worker to drain any queued requests). When a poll fails, the index
/// is marked offline and the poller backs off before retrying.
pub async fn run(index: Arc<DriveIndex>, shutdown: CancellationToken) {
    tracing::info!("event poller started, waiting for bootstrap...");

    // Wait for bootstrap to complete (sets volume_id)
    tokio::select! {
        _ = shutdown.cancelled() => {
            tracing::info!("event poller shutting down before bootstrap");
            return;
        }
        _ = index.bootstrap_done.notified() => {}
    }

    let volume_id = match index.volume_id().await {
        Some(vid) => vid,
        None => {
            tracing::error!("event poller: no volume_id after bootstrap");
            return;
        }
    };

    // Obtain the initial event cursor
    let mut event_id = loop {
        match index
            .drive_client()
            .get_volume_latest_event_id(volume_id.clone())
            .await
        {
            Ok(id) => {
                tracing::info!(event_id = %id, "initial event cursor obtained");
                index.set_online(true).await;
                break id;
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not get latest event id, will retry");
                index.set_online(false).await;

                tokio::select! {
                    _ = shutdown.cancelled() => {
                        tracing::info!("event poller shutting down during init retry");
                        return;
                    }
                    _ = tokio::time::sleep(OFFLINE_RETRY_INTERVAL) => {}
                }
            }
        }
    };

    loop {
        let sleep_duration = if index.is_online() {
            POLL_INTERVAL
        } else {
            OFFLINE_RETRY_INTERVAL
        };

        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("event poller shutting down");
                break;
            }
            _ = tokio::time::sleep(sleep_duration) => {}
        }

        if shutdown.is_cancelled() {
            break;
        }

        match index
            .drive_client()
            .poll_volume_events(volume_id.clone(), &event_id)
            .await
        {
            Ok(response) => {
                // Successful poll → we're online
                index.set_online(true).await;

                if !response.events.is_empty() {
                    tracing::info!(count = response.events.len(), "received volume events");
                    index.apply_events(&response.events).await;
                }
                event_id = response.event_id;

                if response.refresh {
                    tracing::info!("server requested full refresh — invalidating all children");
                    let mut store = index.store.write().await;
                    store.invalidate_children(crate::index::store::ROOT_INO);
                }

                // If there is more, loop immediately without sleeping
                if response.more {
                    continue;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "event poll failed — marking offline");
                index.set_online(false).await;
            }
        }
    }
}
