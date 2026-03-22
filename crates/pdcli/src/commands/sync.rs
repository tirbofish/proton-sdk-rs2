use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use parking_lot::Mutex;
use crate::state::ReplState;
use crate::rusqlite_cache::RusqliteCache;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::volume::VolumeId;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::links::LinkId;
use std::path::PathBuf;
use futures::stream::{FuturesUnordered, StreamExt};
use rayon::prelude::*;
use indicatif::{ProgressBar, ProgressStyle};
use fuser::Notifier;
use proton_drive_sdk::utils::PotentialObject;

pub async fn sync_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!("Usage: sync <local_path>");
    }

    let local_root = PathBuf::from(args[0]);
    if !local_root.exists() {
        std::fs::create_dir_all(&local_root)?;
    }

    let (client, volume_id, cache) = {
        let s = state.lock();
        let client = s.get_client().cloned().ok_or_else(|| anyhow::anyhow!("Not authenticated"))?;
        let root_uid = s.get_root_node_uid().cloned().ok_or_else(|| anyhow::anyhow!("Root node not found"))?;
        let cache = s.get_cache().ok_or_else(|| anyhow::anyhow!("Cache not initialized"))?;
        (client, root_uid.volume_id, cache)
    };

    println!("Starting background sync to {}...", local_root.display());
    
    let local_root_clone = local_root.clone();
    let state_clone = state.clone();
    tokio::spawn(async move {
        if let Err(e) = run_sync_loop_with_state(client, volume_id, cache, Some(local_root_clone), state_clone).await {
            eprintln!("\nBackground sync error: {}", e);
        }
    });

    Ok(())
}

/// Minimal startup sync for IndexOnDemand: snapshot the event cursor and
/// populate root children, but skip BFS folder indexing entirely.
pub async fn run_minimal_sync(
    client: &ProtonDriveClient,
    volume_id: &VolumeId,
    cache: &Arc<RusqliteCache>,
    local_root: Option<PathBuf>,
) -> Result<()> {
    let initial_event_id = match cache.get_sync_state(volume_id)? {
        Some((id, _)) => id,
        None => {
            let sp = crate::commands::helpers::new_spinner("Fetching latest event cursor...");
            let id = client.get_volume_latest_event_id(volume_id.clone()).await?;
            sp.finish_and_clear();
            id
        }
    };

    if cache.list_children(volume_id, None)?.is_empty() {
        let sp = crate::commands::helpers::new_spinner("Fetching root folder contents...");
        let root = client.list_children(volume_id.clone(), None).await?;
        let batch: Vec<_> = root
            .into_iter()
            .filter_map(|res| res.result().ok().map(|n| (n, false)))
            .collect();
        sp.finish_and_clear();
        cache.upsert_nodes_batch(&batch)?;
    }

    cache.set_sync_state(volume_id, &initial_event_id, local_root.as_deref())?;
    Ok(())
}

/// One-shot initial sync: snapshot the event cursor, populate root children,
/// BFS-index all folders, and index trash. Awaiting this before showing the
/// REPL ensures the user's filesystem is immediately navigable.
pub async fn run_initial_sync(
    client: &ProtonDriveClient,
    volume_id: &VolumeId,
    cache: &Arc<RusqliteCache>,
    local_root: Option<PathBuf>,
    state: &Arc<Mutex<ReplState>>,
) -> Result<()> {
    // Snapshot cursor first so we don't miss events that arrive during indexing.
    let initial_event_id = match cache.get_sync_state(volume_id)? {
        Some((id, _)) => {
            tracing::info!(volume_id = %volume_id.raw(), event_id = %id, "Resuming from saved event cursor");
            id
        }
        None => {
            let sp = crate::commands::helpers::new_spinner("Fetching latest event cursor...");
            let id = client.get_volume_latest_event_id(volume_id.clone()).await?;
            sp.finish_and_clear();
            tracing::info!(volume_id = %volume_id.raw(), event_id = %id, "No saved cursor — using latest event");
            id
        }
    };

    if cache.list_children(volume_id, None)?.is_empty() {
        tracing::debug!(volume_id = %volume_id.raw(), "Root not yet cached — fetching root children");
        let sp = crate::commands::helpers::new_spinner("Fetching root folder contents...");
        let root = client.list_children(volume_id.clone(), None).await?;
        let batch: Vec<_> = root
            .into_iter()
            .filter_map(|res| res.result().ok().map(|n| (n, false)))
            .collect();
        sp.finish_and_clear();
        tracing::debug!(volume_id = %volume_id.raw(), count = batch.len(), "Root children cached");
        cache.upsert_nodes_batch(&batch)?;
    }

    cache.set_sync_state(volume_id, &initial_event_id, local_root.as_deref())?;

    tracing::debug!(volume_id = %volume_id.raw(), "BFS folder indexing started");
    state.lock().set_sync_status(Some("Indexing...".to_string()));

    // Read previously saved progress so the bar starts from where we left off.
    let (already_indexed, known_total) = cache.get_folder_index_progress(volume_id).unwrap_or((0, 0));
    let is_resuming = already_indexed > 0 && known_total > 0;

    if is_resuming {
        println!("  Resuming index — {} of {} folders already indexed.", already_indexed, known_total);
    } else {
        println!("  Indexing files, this may take a while...");
    }

    let pb = ProgressBar::with_draw_target(
        Some(known_total),
        indicatif::ProgressDrawTarget::stderr_with_hz(8),
    );
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  {spinner:.green.bold} [{bar:32.green/black.dim}] {pos}/{len} folders  {msg:.dim}  eta {prefix} | {elapsed_precise}").unwrap()
            .progress_chars("█▉▊▋▌▍▎▏ ")
            .tick_strings(&["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"]),
    );
    pb.set_position(already_indexed);
    pb.set_prefix("—");
    pb.enable_steady_tick(Duration::from_millis(200));
    pb.set_message("");
    pb.println("  \x1b[2m(press Esc to skip)\x1b[0m");

    let cancelled = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));

    // Spawn a raw-mode watcher so the user can press Esc to skip the rest of indexing.
    let cancelled_watcher = cancelled.clone();
    let done_watcher = done.clone();
    let watcher = tokio::task::spawn_blocking(move || {
        if crossterm::terminal::enable_raw_mode().is_err() { return; }
        loop {
            if done_watcher.load(Ordering::Relaxed) { break; }
            if let Ok(true) = crossterm::event::poll(std::time::Duration::from_millis(100)) {
                if let Ok(crossterm::event::Event::Key(key)) = crossterm::event::read() {
                    if key.code == crossterm::event::KeyCode::Esc {
                        cancelled_watcher.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
        let _ = crossterm::terminal::disable_raw_mode();
    });

    index_unindexed_folders(client, volume_id, cache, state, Some(&pb), Some(&cancelled)).await;

    // Signal the watcher to stop and wait for raw-mode cleanup.
    done.store(true, Ordering::Relaxed);
    let _ = watcher.await;

    if cancelled.load(Ordering::Relaxed) {
        pb.finish_and_clear();
        println!("  Indexing paused — progress saved, will resume next session.");
        state.lock().set_sync_status(Some("Up to date (Idle)".to_string()));
        tracing::info!(volume_id = %volume_id.raw(), "Indexing skipped by user (Esc)");
        return Ok(());
    }

    // Switch to spinner style for the trash phase (no known total).
    pb.set_style(
        ProgressStyle::with_template("  {spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"]),
    );
    pb.set_message("Indexing trash...");

    tracing::debug!(volume_id = %volume_id.raw(), "Trash indexing started");
    match client.enumerate_trash().await {
        Ok(trash) => {
            let batch: Vec<_> = trash.into_iter().filter_map(|r| r.ok()).map(|n| (n, true)).collect();
            tracing::info!(volume_id = %volume_id.raw(), count = batch.len(), "Trash indexing complete");
            let _ = cache.upsert_nodes_batch(&batch);
        }
        Err(e) => tracing::warn!(volume_id = %volume_id.raw(), error = %e, "Trash indexing failed"),
    }

    pb.finish_and_clear();
    state.lock().set_sync_status(Some("Up to date (Idle)".to_string()));
    tracing::info!(volume_id = %volume_id.raw(), "Initial sync complete");
    Ok(())
}

/// Index the photos volume root into the pdcli SQLite cache so FUSE readdir
/// returns real file names. Safe to call multiple times — already-cached
/// nodes are upserted (no-op if unchanged).
#[allow(dead_code)]
pub async fn run_photos_index(
    photos: &proton_drive_sdk::photo::ProtonPhotosClient,
    cache: &Arc<RusqliteCache>,
) -> Result<()> {
    use proton_drive_sdk::node::NodeUid;
    let root = photos.get_photos_root_folder().await?;
    let volume_id = photos.get_photos_volume_id().await?;
    let root_link_id = root.base.uid.link_id.clone();

    let children = photos.list_children(volume_id.clone(), Some(root_link_id.clone())).await?;
    let parent_uid = NodeUid::new(volume_id.clone(), root_link_id.clone());
    let batch: Vec<_> = children
        .into_iter()
        .filter_map(|r| r.result().ok().map(|mut n| {
            // Force-set parent so the FUSE readdir query can find these nodes
            // even if the API doesn't return ParentLinkID for photos.
            n.set_parent_uid(Some(parent_uid.clone()));
            (n, false)
        }))
        .collect();
    if !batch.is_empty() {
        cache.upsert_nodes_batch(&batch)?;
        tracing::info!(count = batch.len(), "Photos index: cached root children");
        eprintln!("  [FUSE] Photos indexed ({} item(s)).", batch.len());
    }
    Ok(())
}

/// Index the root folders of all registered computers into the pdcli SQLite
/// cache. Uses the per-device volume_id so FUSE can look them up correctly.
/// Pass the already-fetched `devices` list to avoid a redundant API call.
/// `pb` is updated with the current device/file name as indexing progresses.
pub async fn run_computers_index(
    client: &ProtonDriveClient,
    devices: &[proton_drive_sdk::device_ops::Device],
    cache: &Arc<RusqliteCache>,
    pb: Option<&ProgressBar>,
) -> Result<()> {
    for device in devices {
        if let Some(pb) = pb {
            pb.set_message(format!("Computers  ―  listing '{}'", device.name));
        }
        let children = match client.list_children(device.volume_id.clone(), Some(device.root_uid.link_id.clone())).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(device_id = %device.device_id, error = %e, "Computers index: failed to list root children");
                continue;
            }
        };
        let batch: Vec<_> = children
            .into_iter()
            .filter_map(|r| r.result().ok())
            .map(|n| {
                if let Some(pb) = pb {
                    pb.set_message(format!("Computers  ―  {} / {}", device.name, n.base().name));
                }
                (n, false)
            })
            .collect();
        if !batch.is_empty() {
            cache.upsert_nodes_batch(&batch)?;
            tracing::info!(device = %device.name, count = batch.len(), "Computers index: cached root children");
        }
    }
    Ok(())
}

/// Run `run_computers_index` in a loop, re-indexing computers every 5 minutes.
///
/// Call this from a `tokio::spawn` instead of calling `run_computers_index`
/// directly so the Computers/ directory stays up-to-date while the drive is
/// mounted.
pub async fn run_computers_index_continuous(
    client: ProtonDriveClient,
    devices: Vec<proton_drive_sdk::device_ops::Device>,
    cache: Arc<RusqliteCache>,
    notifier: Option<Arc<Notifier>>,
    comp_count: usize,
) {
    const INTERVAL: Duration = Duration::from_secs(5 * 60);
    const INO_COMPUTERS: u64 = 5;
    const INO_COMP_BASE: u64 = 50;
    loop {
        // Subsequent cycles run silently — no progress bar updates (the bar is
        // already finished from the initial pass in the mount spawn).
        match run_computers_index(&client, &devices, &cache, None).await {
            Ok(()) => {
                if let Some(ref n) = notifier {
                    let _ = n.inval_inode(INO_COMPUTERS, 0, -1);
                    for i in 0..comp_count as u64 {
                        let _ = n.inval_inode(INO_COMP_BASE + i, 0, -1);
                    }
                }
                tracing::info!("Computers index cycle complete.");
            }
            Err(e) => tracing::warn!("Computers index cycle error: {e}"),
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

/// Seed the MyFiles root (if still empty) and then BFS-index all unindexed
/// folders. Safe to call as a background task — creates no progress bar.
pub async fn seed_and_index_myfiles(
    client: &ProtonDriveClient,
    volume_id: &VolumeId,
    cache: &Arc<RusqliteCache>,
    state: &Arc<Mutex<ReplState>>,
    root_link_id: Option<&LinkId>,
    pb: Option<&ProgressBar>,
) {
    // If root has no known children yet, seed it from the API first.
    if cache.list_children(volume_id, root_link_id).unwrap_or_default().is_empty() {
        if let Some(pb) = pb { pb.set_message("MyFiles  ―  loading root..."); }
        match client.list_children(volume_id.clone(), root_link_id.cloned()).await {
            Ok(children) => {
                let batch: Vec<_> = children
                    .into_iter()
                    .filter_map(|r| r.result().ok().map(|n| (n, false)))
                    .collect();
                let _ = cache.upsert_nodes_batch(&batch);
            }
            Err(e) => tracing::warn!("MyFiles root seed: {e}"),
        }
    }
    if let Some(pb) = pb { pb.set_message("MyFiles  ―  indexing folders..."); }
    index_unindexed_folders(client, volume_id, cache, state, None, None).await;
}

/// Index trashed items into the SQLite cache so the Trash virtual directory
/// in FUSE shows real nodes.
pub async fn run_trash_index(
    client: &ProtonDriveClient,
    _volume_id: &VolumeId,
    cache: &Arc<RusqliteCache>,
) -> Result<()> {
    let trash = client.enumerate_trash().await?;
    let batch: Vec<_> = trash
        .into_iter()
        .filter_map(|r| r.ok())
        .map(|n| (n, true))
        .collect();
    if !batch.is_empty() {
        cache.upsert_nodes_batch(&batch)?;
        tracing::info!(count = batch.len(), "Trash index: cached items");
        eprintln!("  [FUSE] Trash indexed ({} item(s)).", batch.len());
    } else {
        eprintln!("  [FUSE] Trash is empty.");
    }
    Ok(())
}

/// Events polling loop — runs indefinitely. Spawn as a background task after
/// `run_initial_sync` has completed.
pub async fn run_events_loop(
    client: ProtonDriveClient,
    volume_id: VolumeId,
    cache: Arc<RusqliteCache>,
    local_root: Option<PathBuf>,
    state: Arc<Mutex<ReplState>>,
    root_link_id: Option<LinkId>,
    notifier: Option<Arc<Notifier>>,
) -> Result<()> {
    let mut current_event_id = match cache.get_sync_state(&volume_id)? {
        Some((id, _)) => id,
        None => client.get_volume_latest_event_id(volume_id.clone()).await?,
    };

    tracing::info!(volume_id = %volume_id.raw(), event_id = %current_event_id, "Events loop started");
    state.lock().set_sync_status(Some("Up to date (Idle)".to_string()));

    loop {
        match client.poll_volume_events(volume_id.clone(), &current_event_id).await {
            Ok(resp) => {
                let event_count = resp.events.len();

                if event_count > 0 {
                    tracing::info!(
                        volume_id = %volume_id.raw(),
                        from_event_id = %current_event_id,
                        next_event_id = %resp.event_id,
                        count = event_count,
                        more = resp.more,
                        refresh = resp.refresh,
                        "Volume events received"
                    );
                    if notifier.is_some() {
                        eprintln!("  [events] {} new event(s) (more={}, refresh={})", event_count, resp.more, resp.refresh);
                    }
                    state.lock().set_sync_status(Some(format!("Processing {} updates", event_count)));
                } else {
                    tracing::trace!(volume_id = %volume_id.raw(), event_id = %current_event_id, "Poll: no new events");
                }

                // Stream event fetches in parallel: separate deletes (sync) from
                // node fetches (async) and drive them concurrently.
                let mut batch = Vec::new();
                let mut delete_ids = Vec::new();
                let mut fetch_events = Vec::new();

                for event in resp.events {
                    match event.event_type {
                        0 => {
                            if notifier.is_some() {
                                eprintln!("  [events] delete link:{}", event.link.link_id.raw());
                            }
                            delete_ids.push(event.link.link_id);
                        }
                        t @ 1..=3 => {
                            if notifier.is_some() {
                                eprintln!("  [events] type={} link:{}", t, event.link.link_id.raw());
                            }
                            fetch_events.push((event.link.link_id, event.link.is_trashed));
                        }
                        n => tracing::warn!(event_type = n, "Unknown event type — skipping"),
                    }
                }

                for link_id in &delete_ids {
                    // Notify kernel before removing from cache so we can still look up name/parent.
                    if let Some(ref n) = notifier {
                        if let Ok(Some(node)) = cache.get_node_by_uid(&volume_id, link_id) {
                            let parent_ino = fuse_parent_ino(
                                &cache, &volume_id,
                                node.parent_link_id.as_deref(),
                                root_link_id.as_ref(),
                            );
                            let _ = n.inval_entry(parent_ino, &OsString::from(&node.name));
                        }
                    }
                    cache.delete_node(&volume_id, link_id)?;
                    tracing::debug!(link_id = %link_id.raw(), "Node deleted from cache");
                }

                let fetch_futures: Vec<_> = fetch_events
                    .into_par_iter()
                    .map(|(link_id, is_trashed)| {
                        let uid = NodeUid::new(volume_id.clone(), link_id.clone());
                        let c = client.clone();
                        (async move { c.get_node(uid).await }, link_id, is_trashed)
                    })
                    .collect();

                let mut fetch_tasks: FuturesUnordered<_> = fetch_futures
                    .into_iter()
                    .map(|(fut, link_id, is_trashed)| async move {
                        (fut.await, link_id, is_trashed)
                    })
                    .collect();

                while let Some((result, link_id, is_trashed)) = fetch_tasks.next().await {
                    match result {
                        Ok(PotentialObject::Node(node)) => {
                            tracing::debug!(link_id = %link_id.raw(), name = %node.base().name, is_trashed, "Node upserted");
                            batch.push((node, is_trashed));
                        }
                        Ok(PotentialObject::Degraded(d)) => {
                            tracing::warn!(link_id = %d.uid().link_id.raw(), "Degraded node, skipping");
                        }
                        Err(e) => {
                            tracing::warn!(link_id = %link_id.raw(), error = %e, "Failed to fetch node for event");
                        }
                    }
                }

                // Use the response's top-level EventID as the cursor for the next poll
                current_event_id = resp.event_id.clone();

                if !batch.is_empty() {
                    // Push kernel notifications for changed parent directories so
                    // Nautilus / any FUSE client sees changes without navigate-away.
                    if let Some(ref n) = notifier {
                        // Collect unique parent FUSE inodes to notify.
                        let mut parent_inos: std::collections::HashSet<u64> = std::collections::HashSet::new();
                        for (node, _) in &batch {
                            let base = node.base();
                            let parent_ino = fuse_parent_ino(
                                &cache, &volume_id, base.parent_uid.as_ref().map(|u| u.link_id.raw()),
                                root_link_id.as_ref(),
                            );
                            parent_inos.insert(parent_ino);
                            // Also invalidate the node itself if it already has a FUSE inode.
                            if let Ok(Some(cached)) = cache.get_node_by_uid(&volume_id, &base.uid.link_id) {
                                if let Some(ino) = cached.inode {
                                    let _ = n.inval_inode(ino + 100, 0, 0);
                                }
                            }
                        }
                        for parent_ino in parent_inos {
                            let _ = n.inval_inode(parent_ino, 0, 0);
                        }
                    }

                    cache.upsert_nodes_batch(&batch)?;

                    // Index any newly discovered unindexed folders (e.g. from Create events)
                    index_unindexed_folders(&client, &volume_id, &cache, &state, None, None).await;
                }

                if resp.refresh {
                    tracing::info!(volume_id = %volume_id.raw(), "Refresh requested — resetting to latest event");
                    let latest = client.get_volume_latest_event_id(volume_id.clone()).await?;
                    tracing::info!(volume_id = %volume_id.raw(), latest_event_id = %latest, "Cursor reset");
                    current_event_id = latest;
                } else if !resp.more {
                    cache.set_sync_state(&volume_id, &current_event_id, local_root.as_deref())?;
                    state.lock().set_sync_status(Some("Up to date (Idle)".to_string()));
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
            Err(e) => {
                tracing::warn!(volume_id = %volume_id.raw(), error = %e, "Failed to poll volume events, retrying in 10s");
                state.lock().set_sync_status(Some("Connection error, retrying...".to_string()));
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        }
    }
}

/// Convenience wrapper used by the `sync` REPL command: runs initial sync in a
/// background task and then drives the events loop until the process exits.
pub async fn run_sync_loop_with_state(
    client: ProtonDriveClient,
    volume_id: VolumeId,
    cache: Arc<RusqliteCache>,
    local_root: Option<PathBuf>,
    state: Arc<Mutex<ReplState>>,
) -> Result<()> {
    // Spawn initial sync in background so events start flowing immediately.
    {
        let c = client.clone();
        let v = volume_id.clone();
        let ca = cache.clone();
        let lr = local_root.clone();
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = run_initial_sync(&c, &v, &ca, lr, &st).await {
                tracing::warn!(error = %e, "Background initial sync failed");
            }
        });
    }
    run_events_loop(client, volume_id, cache, local_root, state, None, None).await
}

/// Map a node's parent_link_id raw string to the FUSE inode that represents it.
/// Virtual dir constants must match those in fuse.rs.
fn fuse_parent_ino(
    cache: &RusqliteCache,
    volume_id: &VolumeId,
    parent_link_id_raw: Option<&str>,
    root_link_id: Option<&LinkId>,
) -> u64 {
    const INO_MYFILES: u64 = 2;
    const INO_START: u64 = 100;

    let Some(pid) = parent_link_id_raw else {
        return INO_MYFILES;
    };
    if root_link_id.map(|r| r.raw() == pid).unwrap_or(false) {
        return INO_MYFILES;
    }
    let parent_lid = LinkId::new(pid.to_string());
    if let Ok(Some(p)) = cache.get_node_by_uid(volume_id, &parent_lid) {
        if let Some(ino) = p.inode {
            return ino + INO_START;
        }
    }
    INO_MYFILES
}

/// Index any unindexed folders currently in the DB cache. Called after events and at startup.
/// Pass `Some(pb)` to show a live progress bar during the initial startup phase.
///
/// Uses a continuous work queue (max 150 concurrent) rather than batch-then-re-query, so
/// newly discovered sub-folders are submitted immediately as their parent resolves instead
/// of waiting for the current batch of 50 to fully drain.
pub async fn index_unindexed_folders(
    client: &ProtonDriveClient,
    volume_id: &VolumeId,
    cache: &RusqliteCache,
    state: &Arc<Mutex<ReplState>>,
    pb: Option<&ProgressBar>,
    cancelled: Option<&AtomicBool>,
) {
    // Seed total from what's already in the DB so the "N nodes" counter doesn't
    // reset to 0 on every resume.
    let mut total_indexed: u64 = cache
        .get_cached_node_count(volume_id)
        .unwrap_or(0);
    const MAX_CONCURRENT: usize = 60;

    tracing::debug!(volume_id = %volume_id.raw(), "Starting folder indexing");

    // Seed the work queue from the database.  On first run this is just the root's
    // children; on resume it includes every folder not yet marked indexed.
    let initial = match cache.get_unindexed_folders(volume_id) {
        Ok(u) => u,
        Err(e) => { tracing::error!(error = %e, "Failed to query unindexed folders"); return; }
    };

    if initial.is_empty() {
        tracing::info!(volume_id = %volume_id.raw(), "Nothing to index");
        return;
    }

    if let Some(pb) = pb {
        pb.inc_length(initial.len() as u64);
    }

    // `queued` prevents duplicate work.  Pre-populate with already-indexed folders
    // so that child folders discovered during this run don't re-enter the queue if
    // the DB already has them marked done (avoids exponential replay on resume).
    let already_done: HashSet<proton_drive_sdk::links::LinkId> = {
        match cache.get_indexed_folder_ids(volume_id) {
            Ok(ids) => ids.into_iter().collect(),
            Err(_) => HashSet::new(),
        }
    };
    let mut queued: HashSet<proton_drive_sdk::links::LinkId> =
        already_done.union(&initial.iter().map(|(id, _)| id.clone()).collect()).cloned().collect();
    let mut work: VecDeque<(proton_drive_sdk::links::LinkId, String)> = initial.into();

    let mut in_flight: FuturesUnordered<_> = FuturesUnordered::new();

    loop {
        // Fill the concurrent pool up to MAX_CONCURRENT.
        while in_flight.len() < MAX_CONCURRENT {
            if let Some((link_id, name)) = work.pop_front() {
                let c = client.clone();
                let vid = volume_id.clone();
                in_flight.push(async move {
                    // Per-folder timeout: if a folder stalls (e.g. due to rate-limit
                    // backoff), give up after 90 s and leave it unindexed for next run.
                    let res = tokio::time::timeout(
                        Duration::from_secs(90),
                        c.list_children(vid, Some(link_id.clone())),
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("timeout")));
                    (res, link_id, name)
                });
            } else {
                break;
            }
        }

        if in_flight.is_empty() {
            break;
        }

        // Wait for the next completed folder OR a 500 ms heartbeat tick —
        // whichever comes first. The heartbeat keeps the message live while
        // slow/large folders are still being fetched, so the user can see
        // how many requests are still in-flight.
        let completed = tokio::select! {
            res = in_flight.next() => res,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                // Check for Esc-skip every heartbeat.
                if cancelled.map_or(false, |c| c.load(Ordering::Relaxed)) {
                    break;
                }
                if let Some(pb) = pb {
                    // Cap ETA: anything over 2 days is an unreliable estimate — show — instead.
                    let eta = pb.eta();
                    let eta_str = if eta.as_secs() > 2 * 24 * 3600 {
                        "—".to_string()
                    } else {
                        indicatif::HumanDuration(eta).to_string()
                    };
                    pb.set_prefix(eta_str);
                    pb.set_message(format!(
                        "({} nodes • {} fetching)",
                        total_indexed, in_flight.len()
                    ));
                }
                continue;
            }
        };

        if let Some((res, link_id, name)) = completed {
            let nodes_to_write: Vec<(Node, bool)>;

            match res {
                Ok(children) => {
                    let new_nodes: Vec<(Node, bool)> = children
                        .into_par_iter()
                        .filter_map(|cr| cr.result().ok().map(|n| (n, false)))
                        .collect();

                    for (node, _) in &new_nodes {
                        if matches!(node, Node::Folder(_) | Node::Album(_)) {
                            let child_id = node.uid().link_id.clone();
                            if queued.insert(child_id.clone()) {
                                let child_name = node.base().name.clone();
                                work.push_back((child_id, child_name));
                                if let Some(pb) = pb {
                                    pb.inc_length(1);
                                }
                            }
                        }
                    }

                    total_indexed += new_nodes.len() as u64;
                    nodes_to_write = new_nodes;
                }
                Err(e) => {
                    let msg = e.to_string();
                    let is_rate_limited = msg.contains("429")
                        || msg.contains("Too Many Requests")
                        || msg.contains("rate limit")
                        || msg.contains("timeout");

                    if is_rate_limited {
                        if let Some(pb) = pb {
                            pb.println(format!(
                                "    \x1b[33m⚠ rate limited / timeout — '{}/' will retry next run\x1b[0m",
                                name
                            ));
                        }
                        tracing::warn!(folder = %name, error = %e, "Rate limited or timed out indexing folder");
                    } else {
                        if let Some(pb) = pb {
                            pb.println(format!(
                                "    \x1b[31m✗ error indexing '{}/' — {}\x1b[0m",
                                name, e
                            ));
                        }
                        tracing::warn!(folder = %name, error = %e, "Failed to index folder");
                    }
                    // Don't mark this folder indexed — it stays in the queue for next run
                    if let Some(pb) = pb { pb.inc(1); }
                    state.lock().set_sync_status(Some(format!("Indexing: {} nodes", total_indexed)));
                    continue;
                }
            }

            if !nodes_to_write.is_empty() {
                let _ = cache.upsert_nodes_batch(&nodes_to_write);
            }
            let _ = cache.mark_folders_indexed_batch(volume_id, std::slice::from_ref(&link_id));

            if let Some(pb) = pb {
                pb.println(format!("    \x1b[2m{}/\x1b[0m", name));
                pb.inc(1);
                pb.set_message(format!("({} nodes)", total_indexed));
            }
            state.lock().set_sync_status(Some(format!("Indexing: {} nodes", total_indexed)));
        }
    }

    tracing::info!(volume_id = %volume_id.raw(), total_indexed, "Folder indexing finished");
}

/// Background task that continuously syncs a local folder into a computer's
/// Proton Drive device root.  Runs an initial pass immediately, then repeats
/// every `INTERVAL` seconds to pick up new files written locally.
pub async fn run_computer_folder_sync_continuous(
    client: proton_drive_sdk::client::ProtonDriveClient,
    device_id: String,
    local_path: std::path::PathBuf,
) {
    use crate::commands::helpers::{list_children, progress_bar_for, progress_callback};
    use proton_drive_sdk::node::Node;
    use futures::stream::StreamExt;

    const INTERVAL: Duration = Duration::from_secs(5 * 60);

    tracing::info!(device_id = %device_id, path = %local_path.display(), "Computer folder sync started");

    loop {
        // We need the device's root_uid — look it up fresh each cycle so we
        // handle device re-creation or key rotation transparently.
        let devices = match client.list_devices().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Computer folder sync: list_devices failed: {e}");
                tokio::time::sleep(INTERVAL).await;
                continue;
            }
        };

        let device = match devices.into_iter().find(|d| d.device_id == device_id) {
            Some(d) => d,
            None => {
                tracing::warn!(device_id = %device_id, "Computer folder sync: device not found, skipping cycle");
                tokio::time::sleep(INTERVAL).await;
                continue;
            }
        };

        let root_uid = device.root_uid.clone();

        // List what's already in the remote root so we can skip known files.
        let existing_remote: Vec<Node> = match list_children(&client, root_uid.clone()).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Computer folder sync: list_children failed: {e}");
                tokio::time::sleep(INTERVAL).await;
                continue;
            }
        };

        let remote_names: std::collections::HashSet<String> =
            existing_remote.iter().map(|n| n.base().name.clone()).collect();

        let entries: Vec<_> = match std::fs::read_dir(&local_path) {
            Ok(it) => it.flatten().collect(),
            Err(e) => {
                tracing::warn!(path = %local_path.display(), error = %e, "Computer folder sync: cannot read local dir");
                tokio::time::sleep(INTERVAL).await;
                continue;
            }
        };

        let files: Vec<_> = entries.iter()
            .filter(|e| e.path().is_file())
            .map(|e| e.path())
            .collect();

        let client_c = client.clone();
        let root_uid_c = root_uid.clone();
        let results: Vec<_> = futures::stream::iter(files)
            .map(|path| {
                let client_c = client_c.clone();
                let root_uid_c = root_uid_c.clone();
                let remote_names = remote_names.clone();
                async move {
                    let name = path.file_name().unwrap().to_string_lossy().to_string();
                    if remote_names.contains(&name) { return ('s', name); }
                    let pb = progress_bar_for(&name, 0);
                    match client_c.upload_file(&path, root_uid_c, false, progress_callback(pb.clone())).await {
                        Ok(_) => { pb.finish(); ('u', name) }
                        Err(e) => { pb.finish(); tracing::warn!("Folder sync upload failed for '{name}': {e}"); ('e', name) }
                    }
                }
            })
            .buffer_unordered(4)
            .collect()
            .await;

        let (uploaded, skipped, errors) = results.iter().fold((0usize, 0usize, 0usize), |(u, s, e), (c, _)| {
            match c { 'u' => (u + 1, s, e), 's' => (u, s + 1, e), _ => (u, s, e + 1) }
        });

        for entry in entries.iter().filter(|e| e.path().is_dir()) {
            let sub_path = entry.path();
            let sub_name = sub_path.file_name().unwrap().to_string_lossy().to_string();
            let sub_uid = if let Some(n) = existing_remote.iter().find(|n| n.base().name == sub_name) {
                n.uid().clone()
            } else {
                match client.create_folder(root_uid.clone(), sub_name.clone(), None).await {
                    Ok(f) => f.base.uid,
                    Err(e) => {
                        tracing::warn!("Computer folder sync: create_folder '{sub_name}' failed: {e}");
                        continue;
                    }
                }
            };
            sync_subfolder_recursive(&client, &sub_path, sub_uid).await;
        }

        tracing::info!(
            device_id = %device_id,
            uploaded,
            skipped,
            errors,
            "Computer folder sync cycle complete"
        );

        tokio::time::sleep(INTERVAL).await;
    }
}

fn sync_subfolder_recursive<'a>(
    client: &'a proton_drive_sdk::client::ProtonDriveClient,
    local_path: &'a std::path::Path,
    remote_uid: proton_drive_sdk::node::NodeUid,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    use crate::commands::helpers::{list_children, progress_bar_for, progress_callback};
    use futures::stream::StreamExt;

    Box::pin(async move {
        let existing: Vec<proton_drive_sdk::node::Node> =
            match list_children(client, remote_uid.clone()).await {
                Ok(v) => v,
                Err(_) => return,
            };
        let remote_names: std::collections::HashSet<String> =
            existing.iter().map(|n| n.base().name.clone()).collect();

        let entries: Vec<_> = match std::fs::read_dir(local_path) {
            Ok(it) => it.flatten().collect(),
            Err(_) => return,
        };

        let files: Vec<_> = entries.iter()
            .filter(|e| e.path().is_file())
            .map(|e| e.path())
            .collect();

        let _: Vec<_> = futures::stream::iter(files)
            .map(|path| {
                let client_c = client.clone();
                let remote_uid = remote_uid.clone();
                let remote_names = remote_names.clone();
                async move {
                    let name = path.file_name().unwrap().to_string_lossy().to_string();
                    if remote_names.contains(&name) { return; }
                    let pb = progress_bar_for(&name, 0);
                    match client_c.upload_file(&path, remote_uid, false, progress_callback(pb.clone())).await {
                        Ok(_) => { pb.finish(); }
                        Err(e) => { pb.finish(); tracing::warn!("Subfolder sync upload failed for '{name}': {e}"); }
                    }
                }
            })
            .buffer_unordered(4)
            .collect()
            .await;

        for entry in entries.iter().filter(|e| e.path().is_dir()) {
            let sub_path = entry.path();
            let sub_name = sub_path.file_name().unwrap().to_string_lossy().to_string();
            let sub_uid = if let Some(n) = existing.iter().find(|n| n.base().name == sub_name) {
                n.uid().clone()
            } else {
                match client.create_folder(remote_uid.clone(), sub_name, None).await {
                    Ok(f) => f.base.uid,
                    Err(_) => continue,
                }
            };
            sync_subfolder_recursive(client, &sub_path, sub_uid).await;
        }
    })
}

