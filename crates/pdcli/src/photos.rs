use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::Context;
use futures::StreamExt;
use proton_drive_sdk::api::file::photos::AlbumInfo;
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::node::NodeType;
use proton_drive_sdk::node::file::FileThumbnail;
use proton_drive_sdk::node::file::FileUploadMetadata;
use proton_drive_sdk::node::photo::PhotosFileUploadMetadata;
use proton_drive_sdk::node::photo::{PhotoTag, TimelineEntry};
use proton_drive_sdk::node::thumbnail::ThumbnailType;
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::photo::ProtonPhotosClient;
use proton_drive_sdk::utils::PotentialObject;
use proton_drive_sdk::volume::VolumeId;
use serde::Serialize;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::{
    daemon,
    flags::{PhotoDuplicateStrategy, PhotosCommand},
    takeout,
};

const MAX_THUMBNAIL_BYTES: usize = 8 * 1024 * 1024;

#[derive(Serialize)]
struct Photo {
    uid: String,
    capture_time: chrono::DateTime<chrono::Utc>,
    tags: Vec<PhotoTag>,
    name: Option<String>,
    media_type: Option<String>,
    size: Option<i64>,
}

#[derive(Serialize)]
struct TimelinePage {
    items: Vec<Photo>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct Album {
    uid: String,
    name: Option<String>,
    photo_count: u64,
    last_activity_time: chrono::DateTime<chrono::Utc>,
    cover_uid: Option<String>,
}

#[derive(Serialize)]
struct AlbumList {
    albums: Vec<Album>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct AlbumDetails {
    album: AlbumSelection,
    items: Vec<AlbumPhoto>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct AlbumSelection {
    uid: String,
    name: Option<String>,
}

#[derive(Serialize)]
struct AlbumPhoto {
    uid: String,
    capture_time: chrono::DateTime<chrono::Utc>,
    name: Option<String>,
    media_type: Option<String>,
    size: Option<i64>,
}

fn file_metadata(
    uid: &NodeUid,
    nodes: &HashMap<NodeUid, PotentialObject<Node, DegradedNode>>,
) -> (Option<String>, Option<String>, Option<i64>) {
    match nodes.get(uid) {
        Some(PotentialObject::Node(Node::Photo(file) | Node::File(file))) => (
            Some(file.base.base.name.clone()),
            Some(file.base.media_type.clone()),
            file.active_revision.claimed_size,
        ),
        _ => (None, None, None),
    }
}

fn nodes_by_uid(
    nodes: Vec<PotentialObject<Node, DegradedNode>>,
) -> HashMap<NodeUid, PotentialObject<Node, DegradedNode>> {
    nodes
        .into_iter()
        .map(|node| {
            let uid = match &node {
                PotentialObject::Node(node) => node.uid(),
                PotentialObject::Degraded(node) => node.uid(),
            };
            (uid.clone(), node)
        })
        .collect()
}

fn photo(
    entry: TimelineEntry,
    nodes: &HashMap<NodeUid, PotentialObject<Node, DegradedNode>>,
) -> Photo {
    let (name, media_type, size) = file_metadata(&entry.uid, nodes);
    Photo {
        uid: entry.uid.raw(),
        capture_time: entry.capture_time,
        tags: entry.tags,
        name,
        media_type,
        size,
    }
}

fn album(info: AlbumInfo, name: Option<String>) -> Album {
    Album {
        uid: info.uid.raw(),
        name,
        photo_count: info.photo_count,
        last_activity_time: info.last_activity_time,
        cover_uid: info.cover_uid.map(|uid| uid.raw()),
    }
}

async fn album_name(photos: &ProtonPhotosClient, uid: NodeUid) -> anyhow::Result<Option<String>> {
    match photos.get_node(uid).await? {
        PotentialObject::Node(Node::Album(folder)) => Ok(Some(folder.base.name)),
        PotentialObject::Degraded(_) => Ok(None),
        _ => anyhow::bail!("node is not an album"),
    }
}

fn page_cursor(value: Option<String>) -> anyhow::Result<Option<LinkId>> {
    value
        .map(|value| {
            anyhow::ensure!(
                !value.trim().is_empty()
                    && !value.contains(['&', '?', '#'])
                    && !value.chars().any(char::is_control),
                "invalid page cursor"
            );
            Ok(LinkId::new(value))
        })
        .transpose()
}

fn validate_album_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.trim().is_empty() && !name.contains(['/', '\\', '\0']) && name != "." && name != "..",
        "invalid album name"
    );
    Ok(())
}

fn photo_uid(value: &str, volume_id: &VolumeId) -> anyhow::Result<NodeUid> {
    let uid = NodeUid::parse(value).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        !uid.volume_id.raw().is_empty() && !uid.link_id.raw().is_empty(),
        "empty photo UID component"
    );
    anyhow::ensure!(
        uid.volume_id == *volume_id,
        "photo is not in the Photos volume"
    );
    Ok(uid)
}

fn thumbnail_bytes(uid: &NodeUid, item: FileThumbnail) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        item.file_uid == *uid,
        "thumbnail belongs to a different photo"
    );
    let bytes = match item.result {
        PotentialObject::Node(bytes) => bytes,
        PotentialObject::Degraded(error) => anyhow::bail!("thumbnail unavailable: {error}"),
    };
    anyhow::ensure!(!bytes.is_empty(), "thumbnail is empty");
    anyhow::ensure!(
        bytes.len() <= MAX_THUMBNAIL_BYTES,
        "thumbnail exceeds {MAX_THUMBNAIL_BYTES} bytes"
    );
    Ok(bytes)
}

#[derive(Serialize)]
struct DuplicatePhoto {
    uid: String,
    name: Option<String>,
    size: Option<i64>,
}

#[derive(Default)]
struct PhotoIndex {
    scanned: usize,
    unchecked: Vec<String>,
    extra_copies: usize,
    by_hash: HashMap<Vec<u8>, Vec<DuplicatePhoto>>,
}

impl PhotoIndex {
    fn add_hash(&mut self, uid: &NodeUid, hash: Option<Vec<u8>>) {
        self.scanned += 1;
        if let Some(hash) = hash.filter(|hash| hash.len() == 32) {
            let group = self.by_hash.entry(hash).or_default();
            self.extra_copies += usize::from(!group.is_empty());
            group.push(DuplicatePhoto {
                uid: uid.raw(),
                name: None,
                size: None,
            });
        } else {
            self.unchecked.push(uid.raw());
        }
    }

    fn duplicate_uids(&self) -> anyhow::Result<Vec<NodeUid>> {
        self.by_hash
            .values()
            .filter(|group| group.len() > 1)
            .flatten()
            .map(|photo| NodeUid::parse(&photo.uid).map_err(anyhow::Error::msg))
            .collect()
    }

    fn removal_plan(&self) -> (Vec<String>, Vec<String>) {
        let mut groups: Vec<Vec<_>> = self
            .by_hash
            .values()
            .filter(|group| group.len() > 1)
            .map(|group| {
                let mut uids: Vec<_> = group.iter().map(|photo| photo.uid.clone()).collect();
                uids.sort();
                uids
            })
            .collect();
        groups.sort_by(|a, b| a[0].cmp(&b[0]));
        let kept = groups.iter().map(|group| group[0].clone()).collect();
        let extras = groups
            .into_iter()
            .flat_map(|group| group.into_iter().skip(1))
            .collect();
        (kept, extras)
    }

    fn report(self) -> serde_json::Value {
        let mut groups: Vec<_> = self
            .by_hash
            .into_values()
            .filter(|items| items.len() > 1)
            .collect();
        for group in &mut groups {
            group.sort_by(|a, b| a.uid.cmp(&b.uid));
        }
        groups.sort_by(|a, b| a[0].uid.cmp(&b[0].uid));
        let extra_copies: usize = groups.iter().map(|items| items.len() - 1).sum();
        serde_json::json!({
            "scope": "photos_timeline",
            "comparison": "timeline_content_hmac_sha256",
            "scanned_photos": self.scanned,
            "unchecked_photos": self.unchecked,
            "duplicate_groups": groups.len(),
            "extra_copies": extra_copies,
            "groups": groups,
            "actions": [
                "Keep all copies; this report changes nothing.",
                "Review names, UIDs, favorites and album membership before choosing copies to remove.",
                "Automatically keep one per group and trash extras: pdcli photos dedupe. Preview with --dry-run.",
                "Move selected copies to trash: pdcli photos trash VOLUME~LINK [VOLUME~LINK ...]. Restore through Proton Drive if needed.",
                "Prevent future copies: pdcli photos import PATH --duplicate-strategy skip."
            ]
        })
    }
}

struct ScanProgress {
    visible: bool,
}

impl ScanProgress {
    fn new() -> Self {
        let progress = Self {
            visible: std::io::stderr().is_terminal(),
        };
        progress.discovering(0);
        progress
    }

    fn draw(&self, message: &str) {
        if self.visible {
            let mut stderr = std::io::stderr().lock();
            let _ = write!(stderr, "\r\x1b[2K{message}");
            let _ = stderr.flush();
        }
    }

    fn discovering(&self, discovered: usize) {
        self.draw(&format!(
            "Loading photo timeline… {discovered} photos found"
        ));
    }

    fn checking(&self, checked: usize, total: usize, extra_copies: usize) {
        let filled = if total == 0 { 30 } else { checked * 30 / total };
        let percentage = if total == 0 {
            100
        } else {
            checked * 100 / total
        };
        self.draw(&format!(
            "[{}{}] {percentage:3}% — {checked}/{total} duplicate details loaded, {extra_copies} extra copies",
            "=".repeat(filled), " ".repeat(30 - filled)
        ));
    }
}

impl Drop for ScanProgress {
    fn drop(&mut self) {
        if self.visible {
            let _ = writeln!(std::io::stderr().lock());
        }
    }
}

async fn scan_photo_library(photos: &ProtonPhotosClient) -> anyhow::Result<PhotoIndex> {
    let progress = ScanProgress::new();
    let volume = photos.get_photos_volume_id().await?;
    let mut index = PhotoIndex::default();
    let mut cursor = None;
    let mut seen_cursors = HashSet::new();
    let mut seen_uids = HashSet::new();
    loop {
        let (entries, next) = photos
            .get_timeline_hash_page(&volume, cursor.as_ref())
            .await?;
        for (uid, hash) in entries {
            if seen_uids.insert(uid.clone()) {
                index.add_hash(&uid, hash);
            }
        }
        progress.draw(&format!(
            "[{}>{}] Scanning photo hashes… {} checked, {} extra copies",
            "=".repeat((index.scanned / 500) % 29),
            " ".repeat(29 - (index.scanned / 500) % 29),
            index.scanned,
            index.extra_copies
        ));
        let Some(next) = next else { break };
        anyhow::ensure!(
            seen_cursors.insert(next.raw().to_owned()),
            "Photos timeline cursor did not advance"
        );
        cursor = Some(next);
    }
    Ok(index)
}

async fn duplicate_report(
    photos: &ProtonPhotosClient,
    count_only: bool,
) -> anyhow::Result<serde_json::Value> {
    let mut index = scan_photo_library(photos).await?;
    if count_only {
        return Ok(index.report());
    }
    let uids = index.duplicate_uids()?;
    let progress = ScanProgress::new();
    progress.checking(0, uids.len(), index.extra_copies);
    // Unique photos need no node metadata. Bound concurrent duplicate-only fetches.
    let mut batches = futures::stream::iter(uids.chunks(50).map(|batch| async move {
        let nodes = photos.enumerate_nodes(batch.to_vec()).await?;
        Ok::<_, anyhow::Error>((batch.len(), nodes))
    }))
    .buffer_unordered(4);
    let mut checked = 0;
    let mut metadata = HashMap::new();
    while let Some(result) = batches.next().await {
        let (count, nodes) = result?;
        metadata.extend(nodes_by_uid(nodes));
        checked += count;
        progress.checking(checked, uids.len(), index.extra_copies);
    }
    for group in index.by_hash.values_mut().filter(|group| group.len() > 1) {
        for item in group {
            let uid = NodeUid::parse(&item.uid).map_err(anyhow::Error::msg)?;
            let (name, _, size) = file_metadata(&uid, &metadata);
            item.name = name;
            item.size = size;
        }
    }
    Ok(index.report())
}

async fn dedupe_photos(photos: &ProtonPhotosClient, dry_run: bool) -> anyhow::Result<()> {
    let index = scan_photo_library(photos).await?;
    let (kept, extras) = index.removal_plan();
    let mut report = serde_json::json!({
        "dry_run": dry_run,
        "scope": "photos_timeline",
        "scanned_photos": index.scanned,
        "unchecked_photos": index.unchecked,
        "duplicate_groups": kept.len(),
        "extra_copies": extras.len(),
        "keeper_rule": "lowest_uid_per_group",
        "kept_uids": kept,
        "extra_uids": extras,
        "trashed_uids": [],
        "errors": [],
    });
    if dry_run || extras.is_empty() {
        println!("{report}");
        return Ok(());
    }
    let uids = extras
        .iter()
        .map(|uid| NodeUid::parse(uid).map_err(anyhow::Error::msg))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let progress = ScanProgress::new();
    let mut trashed = Vec::new();
    let mut errors = Vec::new();
    let mut checked = 0;
    for batch in uids.chunks(50) {
        match photos.trash_nodes(batch.to_vec()).await {
            Ok(results) => {
                for uid in batch {
                    let error = match results.get(uid) {
                        Some(Ok(())) => {
                            trashed.push(uid.raw());
                            continue;
                        }
                        Some(Err(error)) => error.to_string(),
                        None => "server returned no result".to_owned(),
                    };
                    errors.push(serde_json::json!({"uid": uid.raw(), "error": error}));
                }
            }
            Err(error) => {
                // Stop after an uncertain batch; never automatically retry a write.
                errors.push(serde_json::json!({"uids": batch.iter().map(NodeUid::raw).collect::<Vec<_>>(), "error": error.to_string(), "result_unknown": true}));
                break;
            }
        }
        checked += batch.len();
        progress.draw(&format!(
            "Moving duplicate copies to trash… {checked}/{} processed",
            uids.len()
        ));
    }
    let failed = !errors.is_empty();
    report["trashed_uids"] = serde_json::to_value(trashed)?;
    report["errors"] = serde_json::to_value(errors)?;
    println!("{report}");
    anyhow::ensure!(
        !failed,
        "some duplicate copies could not be confirmed trashed; see errors in the report"
    );
    Ok(())
}

fn skip_duplicate(
    strategy: PhotoDuplicateStrategy,
    matches: &[DuplicatePhoto],
) -> anyhow::Result<bool> {
    if matches.is_empty() {
        return Ok(false);
    }
    match strategy {
        PhotoDuplicateStrategy::Skip => Ok(true),
        PhotoDuplicateStrategy::Error => anyhow::bail!(
            "photo content already exists as {} ({} matching copies); use --duplicate-strategy skip or keep-both",
            matches[0].uid,
            matches.len()
        ),
        PhotoDuplicateStrategy::KeepBoth => Ok(false),
    }
}

fn supported_photo(path: &Path, include_videos: bool) -> bool {
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    mime.starts_with("image/") || (include_videos && mime.starts_with("video/"))
}

async fn local_photo_paths(
    path: &Path,
    include_videos: bool,
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .context("read import source")?;
    if metadata.is_file() {
        return Ok(vec![path.to_owned()]);
    }
    anyhow::ensure!(
        metadata.is_dir(),
        "import source is not a file or directory"
    );
    let mut directories = vec![path.to_owned()];
    let mut paths = Vec::new();
    while let Some(directory) = directories.pop() {
        let mut entries = tokio::fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let kind = entry.file_type().await?;
            if kind.is_dir() {
                directories.push(entry.path());
            } else if kind.is_file() && supported_photo(&entry.path(), include_videos) {
                paths.push(entry.path());
            }
        }
    }
    paths.sort();
    Ok(paths)
}

async fn import_photos(
    photos: &ProtonPhotosClient,
    path: &Path,
    strategy: PhotoDuplicateStrategy,
    include_videos: bool,
) -> anyhow::Result<()> {
    let paths = local_photo_paths(path, include_videos).await?;
    anyhow::ensure!(
        !paths.is_empty(),
        "no supported photos found in {}",
        path.display()
    );
    let mut index = scan_photo_library(photos).await?;
    anyhow::ensure!(
        index.unchecked.is_empty(),
        "could not check {} photos for duplicates; run pdcli photos duplicates for details",
        index.unchecked.len()
    );
    let progress = ScanProgress::new();
    let mut uploaded = 0;
    let mut skipped = 0;
    for (number, source) in paths.iter().enumerate() {
        progress.draw(&format!(
            "Importing {}/{}: {}",
            number + 1,
            paths.len(),
            source.display()
        ));
        match import_photo(photos, source, strategy, &mut index, include_videos).await {
            Ok(true) => uploaded += 1,
            Ok(false) => skipped += 1,
            Err(error) => {
                println!(
                    "{}",
                    serde_json::json!({"action": "failed", "path": source, "error": format!("{error:#}"), "uploaded": uploaded, "skipped": skipped, "remaining": paths.len() - number - 1})
                );
                return Err(error.context(format!("import {}", source.display())));
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({"action": "summary", "total": paths.len(), "uploaded": uploaded, "skipped": skipped})
    );
    Ok(())
}

async fn import_photo(
    photos: &ProtonPhotosClient,
    path: &Path,
    strategy: PhotoDuplicateStrategy,
    index: &mut PhotoIndex,
    include_videos: bool,
) -> anyhow::Result<bool> {
    let mut file = tokio::fs::File::open(path)
        .await
        .context("open local photo")?;
    let metadata = file.metadata().await?;
    anyhow::ensure!(
        metadata.is_file(),
        "not a local photo file: {}",
        path.display()
    );
    let size = i64::try_from(metadata.len()).context("photo is too large")?;
    anyhow::ensure!(size > 0, "photo is empty: {}", path.display());
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .context("photo name must be valid UTF-8")?
        .to_owned();
    let media_type = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    anyhow::ensure!(
        supported_photo(path, include_videos),
        "not a recognized photo file: {}",
        path.display()
    );

    let digest = hash_photo(&mut file).await?;
    let fingerprint = photos.photo_content_fingerprint(&digest).await?;
    let matches = index
        .by_hash
        .get(&fingerprint)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if skip_duplicate(strategy, matches)? {
        println!(
            "{}",
            serde_json::json!({
                "uid": matches[0].uid, "duplicate": true, "action": "skipped", "path": path,
                "matching_uids": matches.iter().map(|item| &item.uid).collect::<Vec<_>>()
            })
        );
        return Ok(false);
    }
    let duplicate = !matches.is_empty();
    let root = photos.get_photos_root_folder().await?;
    let name = photos
        .drive()
        .get_available_name(root.base.uid.clone(), name, NodeType::Photo)
        .await?;
    let modified = metadata.modified().ok().map(chrono::DateTime::from);
    let mut uploader = photos
        .get_file_uploader(
            root.base.uid,
            name.clone(),
            media_type,
            size,
            PhotosFileUploadMetadata {
                base: FileUploadMetadata {
                    last_modification_time: modified,
                    additional_metadata: None,
                },
                capture_time: modified,
                main_photo_uid: None,
                tags: None,
            },
        )
        .await?;
    uploader.set_expected_sha1(digest.to_vec());
    file.rewind().await?;
    let uid = uploader
        .upload_from_stream(Box::new(file), vec![], Box::new(|_, _| {}))
        .await?;
    println!(
        "{}",
        serde_json::json!({"uid": uid.raw(), "duplicate": duplicate, "action": "uploaded", "name": name, "path": path})
    );
    index.add_hash(&uid, Some(fingerprint));
    Ok(true)
}

async fn hash_photo(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> anyhow::Result<Vec<u8>> {
    let mut hasher = Sha1::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_vec())
}

pub async fn run_cli(force_offline: bool, command: PhotosCommand) -> anyhow::Result<()> {
    if let PhotosCommand::Import {
        path,
        duplicate_strategy,
        include_videos,
    } = command
    {
        anyhow::ensure!(
            !force_offline,
            "Photos import requires a network connection"
        );
        let session = daemon::restore_session(force_offline).await?;
        return import_photos(
            &ProtonPhotosClient::new(&session, None)?,
            &path,
            duplicate_strategy,
            include_videos,
        )
        .await;
    }
    if let PhotosCommand::Export { destination } = command {
        anyhow::ensure!(
            !force_offline,
            "Photos export requires a network connection"
        );
        return takeout::run_photos_cli(force_offline, destination).await;
    }
    anyhow::ensure!(
        !force_offline,
        "Photos browsing requires a network connection"
    );
    let session = daemon::restore_session(force_offline).await?;
    let photos = ProtonPhotosClient::new(&session, None)?;
    match command {
        PhotosCommand::Import { .. } | PhotosCommand::Export { .. } => unreachable!(),
        PhotosCommand::Duplicates { count_only } => {
            println!("{}", duplicate_report(&photos, count_only).await?);
        }
        PhotosCommand::Dedupe { dry_run } => {
            dedupe_photos(&photos, dry_run).await?;
        }
        PhotosCommand::Trash { uids } => {
            let volume = photos.get_photos_volume_id().await?;
            let uids = uids
                .iter()
                .map(|uid| photo_uid(uid, &volume))
                .collect::<anyhow::Result<Vec<_>>>()?;
            // Validate the entire selection before changing any remote node.
            for uid in &uids {
                anyhow::ensure!(
                    matches!(
                        photos.get_node(uid.clone()).await?,
                        PotentialObject::Node(Node::Photo(_))
                    ),
                    "node {uid} is not a readable photo"
                );
            }
            let results = photos.trash_nodes(uids.clone()).await?;
            let mut failed = false;
            for uid in uids {
                let error = match results.get(&uid) {
                    Some(Ok(())) => None,
                    Some(Err(error)) => Some(error.to_string()),
                    None => Some("server returned no result".to_owned()),
                };
                failed |= error.is_some();
                println!(
                    "{}",
                    serde_json::json!({"uid": uid.raw(), "trashed": error.is_none(), "error": error})
                );
            }
            anyhow::ensure!(!failed, "some photos could not be moved to trash");
        }
        PhotosCommand::Timeline { cursor } => {
            anyhow::ensure!(
                cursor.as_ref().is_none_or(|id| !id.is_empty()),
                "empty cursor"
            );
            let volume_id = photos.get_photos_volume_id().await?;
            let cursor = cursor.map(LinkId::new);
            let (entries, next_cursor) = photos
                .get_timeline_page(&volume_id, cursor.as_ref())
                .await?;
            let nodes = nodes_by_uid(
                photos
                    .enumerate_nodes(entries.iter().map(|entry| entry.uid.clone()).collect())
                    .await?,
            );
            let page = TimelinePage {
                items: entries
                    .into_iter()
                    .map(|entry| photo(entry, &nodes))
                    .collect(),
                next_cursor: next_cursor.map(|id| id.raw().to_owned()),
            };
            println!("{}", serde_json::to_string(&page)?);
        }
        PhotosCommand::Albums { cursor } => {
            let cursor = page_cursor(cursor)?;
            let (infos, next_cursor) = photos.get_albums_page(cursor.as_ref()).await?;
            let names: HashMap<_, _> = photos
                .enumerate_nodes(infos.iter().map(|info| info.uid.clone()).collect())
                .await?
                .into_iter()
                .filter_map(|node| match node {
                    PotentialObject::Node(Node::Album(folder)) => {
                        Some((folder.base.uid, folder.base.name))
                    }
                    _ => None,
                })
                .collect();
            let albums = infos
                .into_iter()
                .map(|info| {
                    let name = names.get(&info.uid).cloned();
                    album(info, name)
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string(&AlbumList {
                    albums,
                    next_cursor: next_cursor.map(|id| id.raw().to_owned()),
                })?
            );
        }
        PhotosCommand::Album { uid, cursor } => {
            let volume_id = photos.get_photos_volume_id().await?;
            let uid = photo_uid(&uid, &volume_id)?;
            let cursor = page_cursor(cursor)?;
            let name = album_name(&photos, uid.clone()).await?;
            let (items, next_cursor) = photos.get_album_page(uid.clone(), cursor.as_ref()).await?;
            let nodes = nodes_by_uid(
                photos
                    .enumerate_nodes(items.iter().map(|item| item.uid.clone()).collect())
                    .await?,
            );
            println!(
                "{}",
                serde_json::to_string(&AlbumDetails {
                    album: AlbumSelection {
                        uid: uid.raw(),
                        name,
                    },
                    items: items
                        .into_iter()
                        .map(|item| {
                            let (name, media_type, size) = file_metadata(&item.uid, &nodes);
                            AlbumPhoto {
                                uid: item.uid.raw(),
                                capture_time: item.capture_time,
                                name,
                                media_type,
                                size,
                            }
                        })
                        .collect(),
                    next_cursor: next_cursor.map(|id| id.raw().to_owned()),
                })?
            );
        }
        PhotosCommand::Favorite { uid, off } => {
            let volume_id = photos.get_photos_volume_id().await?;
            let uid = photo_uid(&uid, &volume_id)?;
            photos.favorite_photo(uid.clone(), !off).await?;
            println!(
                "{}",
                serde_json::json!({ "uid": uid.raw(), "favorite": !off })
            );
        }
        PhotosCommand::CreateAlbum { name } => {
            validate_album_name(&name)?;
            let uid = photos.create_album(name).await?;
            println!("{}", serde_json::json!({ "uid": uid.raw() }));
        }
        PhotosCommand::Thumbnail { uid, preview } => {
            let volume_id = photos.get_photos_volume_id().await?;
            let uid = photo_uid(&uid, &volume_id)?;
            let stream = photos
                .drive()
                .enumerate_thumbnails(
                    vec![uid.clone()],
                    if preview {
                        ThumbnailType::Preview
                    } else {
                        ThumbnailType::Thumbnail
                    },
                )
                .await?;
            futures::pin_mut!(stream);
            let item = stream.next().await.context("thumbnail not returned")??;
            let bytes = thumbnail_bytes(&uid, item)?;
            if let Some(item) = stream.next().await {
                item?;
                anyhow::bail!("multiple thumbnails returned for one photo");
            }
            std::io::stdout().lock().write_all(&bytes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use clap::Parser;

    #[tokio::test]
    async fn folder_import_recurses_and_excludes_sidecars_and_videos_by_default() {
        let root =
            std::env::temp_dir().join(format!("pdcli-photo-import-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(root.join("nested"))
            .await
            .unwrap();
        for name in [
            "one.jpg",
            "nested/two.heic",
            "nested/clip.mp4",
            "nested/metadata.aae",
        ] {
            tokio::fs::write(root.join(name), b"test").await.unwrap();
        }
        assert_eq!(local_photo_paths(&root, false).await.unwrap().len(), 2);
        assert_eq!(local_photo_paths(&root, true).await.unwrap().len(), 3);
        assert!(supported_photo(Path::new("photo.JPG"), false));
        assert!(!supported_photo(Path::new("clip.mp4"), false));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[test]
    fn dedupe_plan_keeps_one_copy_per_group_and_leaves_unique_and_unknown_photos_alone() {
        let mut index = PhotoIndex::default();
        for (uid, hash) in [
            ("c", Some(vec![1; 32])),
            ("a", Some(vec![1; 32])),
            ("b", Some(vec![1; 32])),
            ("unique", Some(vec![2; 32])),
            ("unknown", None),
        ] {
            index.add_hash(&NodeUid::from_parts("v", uid), hash);
        }
        assert_eq!(
            index.removal_plan(),
            (
                vec!["v~a".to_owned()],
                vec!["v~b".to_owned(), "v~c".to_owned()]
            )
        );
        assert_eq!(PhotoIndex::default().removal_plan(), (vec![], vec![]));
        for args in [
            vec!["pdcli", "photos", "dedupe"],
            vec!["pdcli", "photos", "dedupe", "--dry-run"],
        ] {
            assert!(crate::flags::Cli::try_parse_from(args).is_ok());
        }
    }

    #[test]
    fn large_library_fetches_metadata_only_for_duplicate_groups() {
        let start = std::time::Instant::now();
        let mut index = PhotoIndex::default();
        for number in 0u64..100_000 {
            let mut hash = vec![0; 32];
            hash[..8].copy_from_slice(&number.to_le_bytes());
            index.add_hash(
                &NodeUid::from_parts("v", &format!("photo-{number}")),
                Some(hash),
            );
        }
        assert!(index.duplicate_uids().unwrap().is_empty());
        for number in 0u64..20 {
            let mut hash = vec![0; 32];
            hash[..8].copy_from_slice(&number.to_le_bytes());
            index.add_hash(
                &NodeUid::from_parts("v", &format!("copy-{number}")),
                Some(hash),
            );
        }
        index.add_hash(&NodeUid::from_parts("v", "invalid"), Some(vec![1; 20]));
        assert_eq!(index.duplicate_uids().unwrap().len(), 40);
        assert_eq!(index.extra_copies, 20);
        assert_eq!(index.unchecked, ["v~invalid"]);
        let report = index.report();
        assert_eq!(report["duplicate_groups"], 20);
        assert_eq!(report["extra_copies"], 20);
        eprintln!(
            "100,020 photo hashes grouped and reported in {:?}; 40 metadata candidates instead of 100,020",
            start.elapsed()
        );
    }

    #[test]
    fn import_duplicate_strategies_skip_fail_or_keep_existing_content() {
        let matches = vec![DuplicatePhoto {
            uid: "volume~original".into(),
            name: Some("original.jpg".into()),
            size: Some(42),
        }];
        assert!(skip_duplicate(PhotoDuplicateStrategy::Skip, &matches).unwrap());
        assert!(skip_duplicate(PhotoDuplicateStrategy::Error, &matches).is_err());
        assert!(!skip_duplicate(PhotoDuplicateStrategy::KeepBoth, &matches).unwrap());
        for strategy in [
            PhotoDuplicateStrategy::Skip,
            PhotoDuplicateStrategy::Error,
            PhotoDuplicateStrategy::KeepBoth,
        ] {
            assert!(!skip_duplicate(strategy, &[]).unwrap());
        }
    }

    #[test]
    fn duplicate_report_counts_extra_copies_regardless_of_names_and_reports_unknowns() {
        let mut index = PhotoIndex::default();
        index.scanned = 4;
        index.by_hash.insert(
            vec![1; 20],
            vec![
                DuplicatePhoto {
                    uid: "v~b".into(),
                    name: Some("renamed.jpg".into()),
                    size: Some(42),
                },
                DuplicatePhoto {
                    uid: "v~a".into(),
                    name: Some("original.jpg".into()),
                    size: Some(42),
                },
                DuplicatePhoto {
                    uid: "v~c".into(),
                    name: Some("third.jpg".into()),
                    size: Some(42),
                },
            ],
        );
        index.by_hash.insert(
            vec![2; 20],
            vec![DuplicatePhoto {
                uid: "v~d".into(),
                name: Some("original.jpg".into()),
                size: Some(42),
            }],
        );
        index.add_hash(&NodeUid::from_parts("v", "missing"), None);
        let report = index.report();
        assert_eq!(report["duplicate_groups"], 1);
        assert_eq!(report["extra_copies"], 2);
        assert_eq!(report["groups"][0][0]["uid"], "v~a");
        assert_eq!(report["unchecked_photos"], serde_json::json!(["v~missing"]));
        assert_eq!(PhotoIndex::default().report()["extra_copies"], 0);
    }

    #[test]
    fn photos_duplicate_commands_parse_strategies_and_require_trash_selection() {
        let cli =
            crate::flags::Cli::try_parse_from(["pdcli", "photos", "import", "photo.jpg"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::flags::Command::Photos {
                command: PhotosCommand::Import {
                    duplicate_strategy: PhotoDuplicateStrategy::Skip,
                    ..
                }
            })
        ));
        for strategy in ["skip", "error", "keep-both"] {
            assert!(
                crate::flags::Cli::try_parse_from([
                    "pdcli",
                    "photos",
                    "import",
                    "photo.jpg",
                    "--duplicate-strategy",
                    strategy
                ])
                .is_ok()
            );
        }
        assert!(
            crate::flags::Cli::try_parse_from([
                "pdcli",
                "photos",
                "import",
                "photo.jpg",
                "--duplicate-strategy",
                "replace"
            ])
            .is_err()
        );
        assert!(crate::flags::Cli::try_parse_from(["pdcli", "photos", "duplicates"]).is_ok());
        assert!(
            crate::flags::Cli::try_parse_from(["pdcli", "photos", "duplicates", "--count-only"])
                .is_ok()
        );
        assert!(crate::flags::Cli::try_parse_from(["pdcli", "photos", "trash"]).is_err());
        assert!(
            crate::flags::Cli::try_parse_from(["pdcli", "photos", "trash", "v~a", "v~b"]).is_ok()
        );
    }

    #[test]
    fn timeline_json_preserves_cursor_metadata_and_nulls() {
        let uid = NodeUid::from_parts("volume", "photo");
        let page = TimelinePage {
            items: vec![photo(
                TimelineEntry {
                    uid,
                    capture_time: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                    tags: vec![PhotoTag::Favorite],
                },
                &HashMap::new(),
            )],
            next_cursor: Some("next".into()),
        };
        let json = serde_json::to_value(page).unwrap();
        assert_eq!(json["next_cursor"], "next");
        assert_eq!(json["items"][0]["uid"], "volume~photo");
        assert_eq!(json["items"][0]["tags"], serde_json::json!([0]));
        assert!(json["items"][0]["name"].is_null());
    }

    #[test]
    fn album_pages_serialize_cursor_and_metadata() {
        let info = AlbumInfo {
            uid: NodeUid::from_parts("volume", "album"),
            photo_count: 501,
            last_activity_time: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            cover_uid: None,
        };
        let json = serde_json::to_value(AlbumList {
            albums: vec![album(info, Some("Summer".into()))],
            next_cursor: Some("next".into()),
        })
        .unwrap();
        assert_eq!(json["albums"][0]["uid"], "volume~album");
        assert_eq!(json["albums"][0]["name"], "Summer");
        assert_eq!(json["albums"][0]["photo_count"], 501);
        assert_eq!(json["next_cursor"], "next");
        let details = serde_json::to_value(AlbumDetails {
            album: AlbumSelection {
                uid: "volume~album".into(),
                name: Some("Summer".into()),
            },
            items: vec![],
            next_cursor: None,
        })
        .unwrap();
        assert!(details["next_cursor"].is_null());
        assert!(details["album"].get("photo_count").is_none());
    }

    #[test]
    fn photos_commands_parse_cursors_and_album_uids() {
        let cli =
            crate::flags::Cli::try_parse_from(["pdcli", "photos", "timeline", "--cursor", "next"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::flags::Command::Photos {
                command: PhotosCommand::Timeline { cursor: Some(cursor) }
            }) if cursor == "next"
        ));
        let cli =
            crate::flags::Cli::try_parse_from(["pdcli", "photos", "albums", "--cursor", "next"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::flags::Command::Photos {
                command: PhotosCommand::Albums { cursor: Some(cursor) }
            }) if cursor == "next"
        ));
        let cli = crate::flags::Cli::try_parse_from([
            "pdcli",
            "photos",
            "album",
            "volume~album",
            "--cursor",
            "next",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::flags::Command::Photos {
                command: PhotosCommand::Album { uid, cursor: Some(cursor) }
            }) if uid == "volume~album" && cursor == "next"
        ));
        assert!(
            crate::flags::Cli::try_parse_from([
                "pdcli",
                "photos",
                "favorite",
                "volume~photo",
                "--off"
            ])
            .is_ok()
        );
        assert!(
            crate::flags::Cli::try_parse_from(["pdcli", "photos", "create-album", "Summer"])
                .is_ok()
        );
        let cli =
            crate::flags::Cli::try_parse_from(["pdcli", "photos", "thumbnail", "volume~photo"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::flags::Command::Photos {
                command: PhotosCommand::Thumbnail { preview: false, .. }
            })
        ));
        let cli = crate::flags::Cli::try_parse_from([
            "pdcli",
            "photos",
            "thumbnail",
            "volume~photo",
            "--preview",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::flags::Command::Photos {
                command: PhotosCommand::Thumbnail { preview: true, .. }
            })
        ));
    }

    #[test]
    fn photos_input_output_require_one_path() {
        let cli = crate::flags::Cli::try_parse_from(["pdcli", "photos", "export", "path"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::flags::Command::Photos {
                command: PhotosCommand::Export { .. }
            })
        ));
        assert!(crate::flags::Cli::try_parse_from(["pdcli", "photos", "export"]).is_err());
        assert!(
            crate::flags::Cli::try_parse_from(["pdcli", "photos", "export", "one", "two"]).is_err()
        );
        let cli = crate::flags::Cli::try_parse_from(["pdcli", "photos", "import", "path"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::flags::Command::Photos {
                command: PhotosCommand::Import { .. }
            })
        ));
        assert!(crate::flags::Cli::try_parse_from(["pdcli", "photos", "import"]).is_err());
        assert!(
            crate::flags::Cli::try_parse_from(["pdcli", "photos", "import", "one", "two"]).is_err()
        );
    }

    #[tokio::test]
    async fn photo_hash_streams_sha1_without_consuming_source() {
        let mut source = std::io::Cursor::new(b"abc".to_vec());
        let hash = hash_photo(&mut source).await.unwrap();
        assert_eq!(
            hash.iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        source.rewind().await.unwrap();
        assert_eq!(source.into_inner(), b"abc");
    }

    #[test]
    fn thumbnail_requires_matching_uid_and_bounded_nonempty_bytes() {
        let volume = VolumeId::new("photos".into());
        let uid = photo_uid("photos~photo", &volume).unwrap();
        for bad in ["invalid", "~photo", "photos~", "drive~photo"] {
            assert!(photo_uid(bad, &volume).is_err());
        }
        let item = |file_uid, bytes| FileThumbnail {
            file_uid,
            result: PotentialObject::Node(bytes),
        };
        assert_eq!(
            thumbnail_bytes(&uid, item(uid.clone(), vec![1, 2])).unwrap(),
            vec![1, 2]
        );
        assert!(
            thumbnail_bytes(&uid, item(NodeUid::from_parts("photos", "other"), vec![1])).is_err()
        );
        assert!(thumbnail_bytes(&uid, item(uid.clone(), vec![])).is_err());
        assert!(
            thumbnail_bytes(&uid, item(uid.clone(), vec![0; MAX_THUMBNAIL_BYTES + 1])).is_err()
        );
    }

    #[test]
    fn album_names_and_page_cursors_are_validated() {
        assert!(validate_album_name("Summer 2026").is_ok());
        for invalid in ["", " ", ".", "..", "a/b", "a\\b", "a\0b"] {
            assert!(validate_album_name(invalid).is_err());
        }
        assert_eq!(
            page_cursor(Some("next".into())).unwrap().unwrap().raw(),
            "next"
        );
        for invalid in ["", " ", "a&b", "a?b", "a#b", "a\nb"] {
            assert!(page_cursor(Some(invalid.into())).is_err());
        }
    }
}
