use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::index::DriveIndex;
use crate::index::requests::RequestKind;

/// Maximum number of consecutive transient failures before we consider
/// ourselves offline and wait for a connectivity signal.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Background worker that picks pending requests from the index and executes
/// them against the Proton Drive API, writing results back into the index.
///
/// When offline, requests pile up in `Pending` / `AwaitingRetry` state. The
/// worker sleeps until `work_available` is notified — which happens when
/// `DriveIndex::set_online(true)` is called (e.g. by the event poller after a
/// successful poll) or when new requests are submitted.
pub async fn run(index: Arc<DriveIndex>, shutdown: CancellationToken) {
    tracing::info!("request worker started");
    let mut consecutive_failures: u32 = 0;

    loop {
        // Wait for work or shutdown
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("request worker shutting down");
                break;
            }
            _ = index.work_available.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }

        if shutdown.is_cancelled() {
            break;
        }

        // Drain all pending requests
        while let Some(req) = index.take_pending_request().await {
            if shutdown.is_cancelled() {
                // Put it back as pending before exiting
                index.retry_later(req.id, "shutdown".into()).await;
                break;
            }

            tracing::info!(id = req.id, kind = ?req.kind, attempt = req.attempts + 1, "processing request");

            let result = execute_request(&index, &req.kind).await;

            match result {
                Ok(()) => {
                    tracing::info!(id = req.id, "request completed");
                    index.complete_request(req.id).await;
                    consecutive_failures = 0;
                    index.set_online(true).await;
                }
                Err(e) if is_transient(&e) => {
                    consecutive_failures += 1;
                    tracing::warn!(
                        id = req.id,
                        error = %e,
                        consecutive = consecutive_failures,
                        "transient failure, will retry when online"
                    );
                    index.retry_later(req.id, e.to_string()).await;

                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        tracing::warn!("too many consecutive failures — marking offline");
                        index.set_online(false).await;
                        // Stop draining; wait for connectivity signal
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!(id = req.id, error = %e, "permanent failure");
                    index.fail_request(req.id, e.to_string()).await;
                }
            }
        }
    }
}

async fn execute_request(
    _index: &DriveIndex,
    kind: &RequestKind,
) -> anyhow::Result<()> {
    match kind {
        RequestKind::Delete { node_uid } => {
            // TODO: call drive_client to trash the node
            tracing::warn!(uid = %node_uid, "Delete not yet wired to API");
            Ok(())
        }
        RequestKind::Rename { node_uid, new_name } => {
            // TODO: call drive_client to rename the node
            tracing::warn!(uid = %node_uid, new_name, "Rename not yet wired to API");
            Ok(())
        }
    }
}

/// Heuristic: treat connection/timeout errors as transient, everything else
/// (4xx, decryption, logic errors) as permanent.
fn is_transient(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("connect")
        || msg.contains("timeout")
        || msg.contains("timed out")
        || msg.contains("dns")
        || msg.contains("network")
        || msg.contains("unreachable")
        || msg.contains("connection refused")
        || msg.contains("connection reset")
        || msg.contains("broken pipe")
        || err.downcast_ref::<std::io::Error>().map_or(false, |io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::NotConnected
            )
        })
}
