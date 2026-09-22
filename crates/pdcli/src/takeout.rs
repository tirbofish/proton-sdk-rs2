use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::device_ops::DeviceOperations;
use proton_drive_sdk::futures::StreamExt;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::photo::ProtonPhotosClient;
use proton_drive_sdk::utils::PotentialObject;

use crate::{computers, daemon};

const MANIFEST_NAME: &str = ".pdcli-takeout-manifest.json";

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Manifest {
    version: u8,
    entries: BTreeMap<String, ManifestEntry>,
    #[serde(default)]
    issues: Vec<ManifestIssue>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ManifestIssue {
    uid: String,
    path: String,
    error: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ManifestEntry {
    path: String,
    revision: String,
    size: u64,
}

#[derive(Default)]
struct Stats {
    exported: usize,
    skipped: usize,
    unsupported: usize,
}

pub async fn run_cli(force_offline: bool, destination: PathBuf) -> anyhow::Result<()> {
    let session = daemon::restore_session(force_offline).await?;
    let drive = ProtonDriveClient::new(&session, None)?;
    let destination = computers::expand_path(&destination.to_string_lossy());
    std::fs::create_dir_all(&destination)?;
    let manifest_path = destination.join(MANIFEST_NAME);
    let mut manifest = load_manifest(&manifest_path)?;
    let mut stats = Stats::default();
    let root = drive.get_my_files_folder().await?;

    export_folder(
        &drive,
        root.base.uid,
        Path::new(""),
        &destination,
        &manifest_path,
        &mut manifest,
        &mut stats,
    )
    .await?;

    export_photos(
        &ProtonPhotosClient::new(&session, None)?,
        &destination,
        &manifest_path,
        &mut manifest,
        &mut stats,
    )
    .await?;
    export_devices(
        &drive,
        &destination,
        &manifest_path,
        &mut manifest,
        &mut stats,
    )
    .await?;

    println!(
        "takeout complete: {} exported, {} already present, {} unsupported",
        stats.exported, stats.skipped, stats.unsupported
    );
    Ok(())
}

async fn export_folder(
    drive: &ProtonDriveClient,
    folder: NodeUid,
    relative_dir: &Path,
    destination: &Path,
    manifest_path: &Path,
    manifest: &mut Manifest,
    stats: &mut Stats,
) -> anyhow::Result<()> {
    let stream = drive.enumerate_folder_children(folder).await?;
    tokio::pin!(stream);
    while let Some(item) = stream.next().await {
        match item? {
            PotentialObject::Node(Node::Folder(node) | Node::Album(node)) => {
                let name = safe_name(&node.base.name);
                let child_relative = relative_dir.join(name);
                std::fs::create_dir_all(destination.join(&child_relative))?;
                Box::pin(export_folder(
                    drive,
                    node.base.uid,
                    &child_relative,
                    destination,
                    manifest_path,
                    manifest,
                    stats,
                ))
                .await?;
            }
            PotentialObject::Node(Node::File(node) | Node::Photo(node)) => {
                export_file(
                    drive,
                    node.base.base.uid,
                    &node.base.base.name,
                    node.active_revision.uid.to_string(),
                    node.active_revision
                        .claimed_size
                        .unwrap_or(node.total_size_on_cloud_storage)
                        .max(0) as u64,
                    relative_dir,
                    destination,
                    manifest_path,
                    manifest,
                    stats,
                )
                .await?;
            }
            PotentialObject::Degraded(node) => {
                record_issue(
                    node.uid(),
                    relative_dir,
                    "node metadata could not be decrypted",
                    manifest_path,
                    manifest,
                    stats,
                )?;
            }
        }
    }
    Ok(())
}

async fn export_photos(
    photos: &ProtonPhotosClient,
    destination: &Path,
    manifest_path: &Path,
    manifest: &mut Manifest,
    stats: &mut Stats,
) -> anyhow::Result<()> {
    let relative_dir = Path::new("Photos");
    std::fs::create_dir_all(destination.join(relative_dir))?;
    let timeline = photos.iterate_timeline().await?;
    let capture_times: HashMap<NodeUid, chrono::DateTime<chrono::Utc>> = timeline
        .iter()
        .map(|item| (item.uid.clone(), item.capture_time))
        .collect();

    for item in photos
        .enumerate_nodes(timeline.into_iter().map(|item| item.uid).collect())
        .await?
    {
        match item {
            PotentialObject::Node(Node::File(node) | Node::Photo(node)) => {
                let photo_dir = capture_times
                    .get(&node.base.base.uid)
                    .map(|time| relative_dir.join(time.format("%Y/%m").to_string()))
                    .unwrap_or_else(|| relative_dir.join("undated"));
                export_file(
                    photos.drive(),
                    node.base.base.uid,
                    &node.base.base.name,
                    node.active_revision.uid.to_string(),
                    node.active_revision
                        .claimed_size
                        .unwrap_or(node.total_size_on_cloud_storage)
                        .max(0) as u64,
                    &photo_dir,
                    destination,
                    manifest_path,
                    manifest,
                    stats,
                )
                .await?;
            }
            PotentialObject::Node(node) => record_issue(
                node.uid(),
                relative_dir,
                "timeline entry is not a photo file",
                manifest_path,
                manifest,
                stats,
            )?,
            PotentialObject::Degraded(node) => record_issue(
                node.uid(),
                relative_dir,
                "photo metadata could not be decrypted",
                manifest_path,
                manifest,
                stats,
            )?,
        }
    }
    Ok(())
}

async fn export_devices(
    drive: &ProtonDriveClient,
    destination: &Path,
    manifest_path: &Path,
    manifest: &mut Manifest,
    stats: &mut Stats,
) -> anyhow::Result<()> {
    let relative_dir = Path::new("Computers");
    std::fs::create_dir_all(destination.join(relative_dir))?;
    for device in DeviceOperations::list_devices(drive).await? {
        let device_dir = relative_dir.join(safe_name(&device.name));
        std::fs::create_dir_all(destination.join(&device_dir))?;
        if let Err(error) = Box::pin(export_folder(
            drive,
            device.root_uid.clone(),
            &device_dir,
            destination,
            manifest_path,
            manifest,
            stats,
        ))
        .await
        {
            record_issue(
                &device.root_uid,
                &device_dir,
                &error.to_string(),
                manifest_path,
                manifest,
                stats,
            )?;
        }
    }
    Ok(())
}

fn record_issue(
    uid: &NodeUid,
    path: &Path,
    error: &str,
    manifest_path: &Path,
    manifest: &mut Manifest,
    stats: &mut Stats,
) -> anyhow::Result<()> {
    stats.unsupported += 1;
    manifest.issues.push(ManifestIssue {
        uid: uid.raw(),
        path: path.to_string_lossy().into_owned(),
        error: error.to_string(),
    });
    save_manifest(manifest_path, manifest)
}

#[allow(clippy::too_many_arguments)]
async fn export_file(
    drive: &ProtonDriveClient,
    uid: NodeUid,
    name: &str,
    revision: String,
    size: u64,
    relative_dir: &Path,
    destination: &Path,
    manifest_path: &Path,
    manifest: &mut Manifest,
    stats: &mut Stats,
) -> anyhow::Result<()> {
    let key = uid.raw();
    let relative = manifest
        .entries
        .get(&key)
        .and_then(|entry| safe_manifest_path(&entry.path))
        .unwrap_or_else(|| {
            unique_relative_path(relative_dir, &safe_name(name), destination, manifest)
        });
    let output = destination.join(&relative);

    if let Some(entry) = manifest.entries.get(&key) {
        if entry.revision == revision
            && entry.size == size
            && output.is_file()
            && std::fs::metadata(&output)
                .map(|meta| meta.len() == size)
                .unwrap_or(false)
        {
            stats.skipped += 1;
            return Ok(());
        }
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = output.with_file_name(format!(
        ".{}.pdcli-partial",
        output
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
    ));
    let _ = std::fs::remove_file(&partial);
    if let Err(error) = drive
        .download_to_file(uid.clone(), &partial, Box::new(|_, _| {}))
        .await
    {
        let _ = std::fs::remove_file(&partial);
        return Err(error);
    }
    if size != 0 && std::fs::metadata(&partial)?.len() != size {
        let actual = std::fs::metadata(&partial)?.len();
        let _ = std::fs::remove_file(&partial);
        anyhow::bail!(
            "downloaded {} with {} bytes; expected {}",
            relative.display(),
            actual,
            size
        );
    }
    std::fs::rename(&partial, &output)?;
    manifest.entries.insert(
        key,
        ManifestEntry {
            path: relative.to_string_lossy().into_owned(),
            revision,
            size,
        },
    );
    save_manifest(manifest_path, manifest)?;
    stats.exported += 1;
    println!("exported {}", relative.display());
    Ok(())
}

fn load_manifest(path: &Path) -> anyhow::Result<Manifest> {
    if !path.exists() {
        return Ok(Manifest {
            version: 1,
            ..Default::default()
        });
    }
    let manifest: Manifest = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    anyhow::ensure!(
        manifest.version == 1,
        "unsupported takeout manifest version"
    );
    Ok(manifest)
}

fn save_manifest(path: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(manifest)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn safe_name(name: &str) -> String {
    let name = name.replace(['/', '\\', '\0'], "_");
    if name.is_empty() || name == "." || name == ".." {
        "unnamed".into()
    } else {
        name
    }
}

fn safe_manifest_path(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        None
    } else {
        Some(path)
    }
}

fn unique_relative_path(
    parent: &Path,
    name: &str,
    destination: &Path,
    manifest: &Manifest,
) -> PathBuf {
    let candidate = parent.join(name);
    let used = |path: &Path| {
        destination.join(path).exists()
            || manifest
                .entries
                .values()
                .any(|entry| safe_manifest_path(&entry.path).as_deref() == Some(path))
    };
    if !used(&candidate) {
        return candidate;
    }
    let (stem, extension) = name
        .rsplit_once('.')
        .filter(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
        .map_or((name, ""), |parts| parts);
    for index in 1..10_000 {
        let name = if extension.is_empty() {
            format!("{stem} ({index})")
        } else {
            format!("{stem} ({index}).{extension}")
        };
        let candidate = parent.join(name);
        if !used(&candidate) {
            return candidate;
        }
    }
    parent.join(format!("{name} (collision)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_paths_cannot_escape_destination() {
        assert!(safe_manifest_path("../outside").is_none());
        assert!(safe_manifest_path("/outside").is_none());
        assert_eq!(safe_name("a/b"), "a_b");

        let manifest = Manifest {
            version: 1,
            entries: BTreeMap::new(),
            issues: vec![ManifestIssue {
                uid: "volume~node".into(),
                path: "Photos".into(),
                error: "cannot decrypt".into(),
            }],
        };
        assert!(
            serde_json::to_string(&manifest)
                .unwrap()
                .contains("cannot decrypt")
        );
    }
}
