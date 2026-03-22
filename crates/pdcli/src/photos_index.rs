//! Continuous background indexer for the Proton Photos volume.
//!
//! Three phases per cycle:
//!   1. Enumerate albums (root children + each album's children) and cache them.
//!   2. Page through the timeline to record capture_time + tags for every photo.
//!   3. Sleep, then repeat to pick up new photos.
//!
//! Phase 2 is resumable: the cursor (last processed LinkID) is persisted in
//! the SQLite `photos_index_state` table.  If the process is killed mid-page,
//! the next run continues from where it left off.

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

use indicatif::ProgressBar;
use proton_drive_sdk::photo::ProtonPhotosClient;
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::volume::VolumeId;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::utils::PotentialObject;
use futures::stream::FuturesUnordered;
use futures::StreamExt;

use crate::rusqlite_cache::RusqliteCache;

/// How long to pause after a complete index cycle before re-checking.
const REINDEX_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Inode constants mirrored from fuse.rs — used to send kernel invalidation.
const INO_PHOTOS: u64 = 4;
const INO_PHOTOS_ALBUMS: u64 = 6;
const INO_PHOTOS_ALL: u64 = 7;
const INO_PHOTOS_FAVS: u64 = 8;
const INO_PHOTOS_VIDEOS: u64 = 9;
const INO_PHOTOS_SCREENSHOTS: u64 = 10;

fn set_msg(pb: Option<&ProgressBar>, msg: impl Into<String>) {
    if let Some(pb) = pb {
        pb.set_message(msg.into());
    }
}

/// Spawn the continuous photos index loop.  Call from a `tokio::spawn`.
///
/// `pb` is a live indicatif `ProgressBar` (held by the mount progress display)
/// that this function updates with the name of each item as it is indexed.
/// Pass `None` if no progress display is active.
pub async fn run_photos_index_continuous(
    photos: ProtonPhotosClient,
    cache: Arc<RusqliteCache>,
    volume_id: VolumeId,
    root_link_id: LinkId,
    notifier: Option<Arc<fuser::Notifier>>,
    pb: Option<Arc<ProgressBar>>,
) {
    let photos = Arc::new(photos);
    loop {
        let pb_ref = pb.as_deref();
        match run_cycle(&photos, &cache, &volume_id, &root_link_id, notifier.as_deref(), pb_ref).await {
            Ok(()) => {
                set_msg(pb_ref, "Photos synced  ―  refreshing in 30 min");
                tracing::info!("Photos index cycle complete.");
            }
            Err(e) => {
                set_msg(pb_ref, format!("Photos error: {e}  ―  retrying in 30 min"));
                tracing::warn!("Photos index cycle error: {e}");
            }
        }
        tokio::time::sleep(REINDEX_INTERVAL).await;
        set_msg(pb_ref, "Photos  ―  starting re-sync...");
    }
}

async fn run_cycle(
    photos: &Arc<ProtonPhotosClient>,
    cache: &Arc<RusqliteCache>,
    volume_id: &VolumeId,
    root_link_id: &LinkId,
    notifier: Option<&fuser::Notifier>,
    pb: Option<&ProgressBar>,
) -> Result<()> {
    // ── Phase 1: Root children (albums + any top-level photos) ────────────
    let root_uid = NodeUid::new(volume_id.clone(), root_link_id.clone());

    set_msg(pb, "Photos  ―  listing albums...");
    let root_stream = photos
        .enumerate_children(volume_id.clone(), Some(root_link_id.clone()))
        .await
        .map_err(|e| {
            set_msg(pb, format!("Photos: failed to list albums: {e}"));
            e
        })?;
    tokio::pin!(root_stream);

    let mut album_uids: Vec<(NodeUid, String)> = Vec::new();
    let mut root_count = 0usize;

    while let Some(item) = root_stream.next().await {
        let item = match item {
            Ok(x) => x,
            Err(e) => {
                set_msg(pb, format!("Photos: stream error: {e}"));
                tracing::warn!("Photos root stream error: {e}");
                continue;
            }
        };
        let PotentialObject::Node(mut node) = item else { continue };
        let is_album = matches!(&node, Node::Album(_));
        let name = node.base().name.clone();
        node.set_parent_uid(Some(root_uid.clone()));

        node = match node { Node::File(f) => Node::Photo(f), n => n };
        if is_album {
            album_uids.push((node.uid().clone(), name.clone()));
        }
        set_msg(pb, format!("Photos  ―  {name}"));
        // Upsert immediately so FUSE can serve it right away.
        if let Err(e) = cache.upsert_nodes_batch(&[(node, false)]) {
            tracing::warn!("Photos: upsert root child '{name}' failed: {e}");
        }
        root_count += 1;
        invalidate(notifier, INO_PHOTOS_ALBUMS);
        invalidate(notifier, INO_PHOTOS_ALL);
        tokio::task::yield_now().await;
    }

    set_msg(pb, format!("Photos  ―  {} album(s) found", album_uids.len()));
    invalidate(notifier, INO_PHOTOS);

    // ── Phase 1b: Each album's children — up to 4 concurrent fetches ──────
    let total_albums = album_uids.len();
    let sem = Arc::new(tokio::sync::Semaphore::new(4));
    let mut tasks: FuturesUnordered<_> = FuturesUnordered::new();

    for (album_uid, album_name) in album_uids {
        let photos = Arc::clone(photos);
        let sem = Arc::clone(&sem);
        let volume_id_c = volume_id.clone();
        tasks.push(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            let nodes = match photos
                .enumerate_children(volume_id_c.clone(), Some(album_uid.link_id.clone()))
                .await
            {
                Ok(stream) => {
                    tokio::pin!(stream);
                    let mut nodes = Vec::new();
                    while let Some(item) = stream.next().await {
                        let Ok(PotentialObject::Node(mut node)) = item else { continue };
                        node = match node { Node::File(f) => Node::Photo(f), n => n };
                        node.set_parent_uid(Some(album_uid.clone()));
                        nodes.push(node);
                    }
                    nodes
                }
                Err(e) => {
                    tracing::warn!("Photos: album '{}': {e}", album_name);
                    Vec::new()
                }
            };
            (album_uid, album_name, nodes)
        });
    }

    let mut total_photos = 0usize;
    let mut completed = 0usize;
    while let Some((album_uid, album_name, nodes)) = tasks.next().await {
        completed += 1;
        let count = nodes.len();
        total_photos += count;
        if !nodes.is_empty() {
            set_msg(pb, format!("Photos  ―  {album_name}: {count} photo(s)  ({completed}/{total_albums} albums, {total_photos} total)"));
            let batch: Vec<_> = nodes.into_iter().map(|n| (n, false)).collect();
            if let Err(e) = cache.upsert_nodes_batch(&batch) {
                tracing::warn!("Photos: upsert album '{album_name}' batch: {e}");
            }
            if let Err(e) = cache.mark_folders_indexed_batch(volume_id, &[album_uid.link_id]) {
                tracing::warn!("Photos: mark indexed album '{album_name}': {e}");
            }
        }
        invalidate(notifier, INO_PHOTOS_ALL);
        invalidate(notifier, INO_PHOTOS_ALBUMS);
    }

    set_msg(pb, format!("Photos  ―  {total_photos} photo(s) indexed, fetching timeline metadata..."));

    // ── Phase 2: Timeline pages  →  capture_time + tags ───────────────────
    let saved = cache.get_timeline_cursor(volume_id)?;
    let mut cursor: Option<LinkId> = saved.map(|s| LinkId::new(s));
    let mut timeline_count: usize = 0;

    loop {
        let (entries, next_cursor) = match photos
            .get_timeline_page(volume_id, cursor.as_ref())
            .await
        {
            Ok(r) => r,
            Err(e) => {
                set_msg(pb, format!("Photos: timeline page error: {e}"));
                tracing::warn!("Photos timeline page error: {e}");
                break;
            }
        };

        if entries.is_empty() {
            break;
        }

        let page_len = entries.len();
        if let Err(e) = cache.upsert_photo_metadata_batch(volume_id, &entries) {
            set_msg(pb, format!("Photos: metadata write error: {e}"));
            tracing::warn!("Photos: upsert_photo_metadata_batch failed: {e}");
        }
        timeline_count += page_len;
        set_msg(pb, format!("Photos  ―  timeline: {timeline_count} metadata entries"));

        let is_last = next_cursor.is_none();
        if let Some(ref nxt) = next_cursor {
            if let Err(e) = cache.set_timeline_cursor(volume_id, nxt.raw()) {
                tracing::warn!("Photos: set_timeline_cursor failed: {e}");
            }
        }
        cursor = next_cursor;

        if is_last {
            if let Err(e) = cache.clear_timeline_cursor(volume_id) {
                tracing::warn!("Photos: clear_timeline_cursor failed: {e}");
            }
            break;
        }

        tokio::task::yield_now().await;
    }

    // Invalidate virtual tag views.
    for ino in [INO_PHOTOS_ALL, INO_PHOTOS_FAVS, INO_PHOTOS_VIDEOS, INO_PHOTOS_SCREENSHOTS] {
        invalidate(notifier, ino);
    }

    tracing::info!(
        root_count,
        total_photos,
        timeline_count,
        "Photos index cycle done."
    );
    Ok(())
}

fn invalidate(notifier: Option<&fuser::Notifier>, ino: u64) {
    if let Some(n) = notifier {
        let _ = n.inval_inode(ino, 0, -1);
    }
}
