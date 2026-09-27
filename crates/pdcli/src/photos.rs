use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use futures::StreamExt;
use proton_drive_sdk::api::file::photos::AlbumInfo;
use proton_drive_sdk::links::LinkId;
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

use crate::{daemon, flags::PhotosCommand, takeout};

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

async fn import_photo(photos: &ProtonPhotosClient, path: &Path) -> anyhow::Result<()> {
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
        media_type.starts_with("image/"),
        "not a recognized image file: {}",
        path.display()
    );

    let digest = hash_photo(&mut file).await?;
    let sha1_hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if let Some(uid) = photos
        .find_photo_duplicates(&name, &sha1_hex)
        .await?
        .first()
    {
        println!(
            "{}",
            serde_json::json!({"uid": uid.raw(), "duplicate": true})
        );
        return Ok(());
    }
    let root = photos.get_photos_root_folder().await?;
    let modified = metadata.modified().ok().map(chrono::DateTime::from);
    let mut uploader = photos
        .get_file_uploader(
            root.base.uid,
            name,
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
        serde_json::json!({"uid": uid.raw(), "duplicate": false})
    );
    Ok(())
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
    if let PhotosCommand::Import { path } = command {
        anyhow::ensure!(
            !force_offline,
            "Photos import requires a network connection"
        );
        let session = daemon::restore_session(force_offline).await?;
        return import_photo(&ProtonPhotosClient::new(&session, None)?, &path).await;
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
