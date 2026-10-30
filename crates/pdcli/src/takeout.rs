use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::device_ops::DeviceOperations;
use proton_drive_sdk::futures::StreamExt;
use proton_drive_sdk::node::revision::RevisionUid;
use proton_drive_sdk::node::{Node, NodeUid, is_proton_document, is_proton_sheet};
use proton_drive_sdk::photo::ProtonPhotosClient;
use proton_drive_sdk::utils::PotentialObject;

use crate::transfer_progress::{Labels, Outcome, Stage, TransferProgress};
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

/// A file found while listing. Downloads start once everything is listed, smallest first.
struct PlannedFile {
    uid: NodeUid,
    name: String,
    revision: RevisionUid,
    size: u64,
    relative_dir: PathBuf,
    from_photos: bool,
    // Computer backups are best effort: a failed file is recorded and the takeout continues.
    record_failure: bool,
}

/// One export into a destination folder, resumable through its manifest.
struct Takeout {
    destination: PathBuf,
    manifest_path: PathBuf,
    manifest: Manifest,
    stats: Stats,
    plan: Vec<PlannedFile>,
    // Photos-only exports write straight into Photos/ without partial files.
    direct_output: bool,
    progress: Arc<TransferProgress>,
}

pub async fn run_cli(force_offline: bool, destination: PathBuf) -> anyhow::Result<()> {
    let session = daemon::restore_session(force_offline).await?;
    let drive = ProtonDriveClient::new(&session, None)?;
    let photos = ProtonPhotosClient::new(&session, None)?;
    let mut takeout = Takeout::open(&destination, false, "files")?;
    let progress = takeout.progress.clone();
    progress
        .drive(async {
            let root = drive.get_my_files_folder().await?;
            takeout
                .list_folder(&drive, root.base.uid, Path::new(""), false)
                .await?;
            takeout.list_photos(&photos).await?;
            takeout.list_devices(&drive).await?;
            takeout.download_all(&drive, photos.drive()).await
        })
        .await?;
    let stats = &takeout.stats;
    progress.log(&format!(
        "takeout complete: {} exported, {} already present, {} unsupported",
        stats.exported, stats.skipped, stats.unsupported
    ));
    Ok(())
}

pub async fn run_photos_cli(force_offline: bool, destination: PathBuf) -> anyhow::Result<()> {
    let session = daemon::restore_session(force_offline).await?;
    let photos = ProtonPhotosClient::new(&session, None)?;
    let mut takeout = Takeout::open(&destination, true, "photos")?;
    let progress = takeout.progress.clone();
    progress
        .drive(async {
            takeout.list_photos(&photos).await?;
            takeout.download_all(photos.drive(), photos.drive()).await
        })
        .await?;
    let stats = &takeout.stats;
    progress.log(&format!(
        "Photos export complete: {} exported, {} already present, {} unsupported",
        stats.exported, stats.skipped, stats.unsupported
    ));
    Ok(())
}

impl Takeout {
    fn open(destination: &Path, direct_output: bool, noun: &'static str) -> anyhow::Result<Self> {
        let destination = computers::expand_path(&destination.to_string_lossy());
        std::fs::create_dir_all(&destination)?;
        let manifest_path = destination.join(MANIFEST_NAME);
        Ok(Self {
            manifest: load_manifest(&manifest_path)?,
            destination,
            manifest_path,
            stats: Stats::default(),
            plan: Vec::new(),
            direct_output,
            progress: Arc::new(TransferProgress::new(Labels {
                verb: "Exporting",
                noun,
                done: "exported",
            })),
        })
    }

    fn plan_file(&mut self, file: PlannedFile) {
        self.plan.push(file);
        self.progress
            .status(Some(format!("listing: {} files found", self.plan.len())));
    }

    async fn list_folder(
        &mut self,
        drive: &ProtonDriveClient,
        folder: NodeUid,
        relative_dir: &Path,
        record_failure: bool,
    ) -> anyhow::Result<()> {
        let stream = drive.enumerate_folder_children(folder).await?;
        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            match item? {
                PotentialObject::Node(Node::Folder(node) | Node::Album(node)) => {
                    let child_relative = relative_dir.join(safe_name(&node.base.name));
                    std::fs::create_dir_all(self.destination.join(&child_relative))?;
                    Box::pin(self.list_folder(
                        drive,
                        node.base.uid,
                        &child_relative,
                        record_failure,
                    ))
                    .await?;
                }
                PotentialObject::Node(Node::File(node) | Node::Photo(node)) => {
                    if unsupported_media_type(&node.base.media_type) {
                        self.record_issue(
                            &node.base.base.uid,
                            relative_dir,
                            "Proton Docs and Sheets are not supported in takeout",
                        )?;
                        continue;
                    }
                    self.plan_file(PlannedFile {
                        uid: node.base.base.uid,
                        name: node.base.base.name.clone(),
                        revision: node.active_revision.uid.clone(),
                        size: node
                            .active_revision
                            .claimed_size
                            .unwrap_or(node.total_size_on_cloud_storage)
                            .max(0) as u64,
                        relative_dir: relative_dir.to_owned(),
                        from_photos: false,
                        record_failure,
                    });
                }
                PotentialObject::Degraded(node) => {
                    self.record_issue(
                        node.uid(),
                        relative_dir,
                        "node metadata could not be decrypted",
                    )?;
                }
            }
        }
        Ok(())
    }

    async fn list_photos(&mut self, photos: &ProtonPhotosClient) -> anyhow::Result<()> {
        let relative_dir = Path::new("Photos");
        std::fs::create_dir_all(self.destination.join(relative_dir))?;
        let volume_id = photos.get_photos_volume_id().await?;
        let mut cursor = None;
        loop {
            let (timeline, next_cursor) = photos
                .get_timeline_page(&volume_id, cursor.as_ref())
                .await?;
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
                        if unsupported_media_type(&node.base.media_type) {
                            self.record_issue(
                                &node.base.base.uid,
                                relative_dir,
                                "Proton Docs and Sheets are not supported in takeout",
                            )?;
                            continue;
                        }
                        let photo_dir = capture_times
                            .get(&node.base.base.uid)
                            .map(|time| relative_dir.join(time.format("%Y/%m").to_string()))
                            .unwrap_or_else(|| relative_dir.join("undated"));
                        self.plan_file(PlannedFile {
                            uid: node.base.base.uid,
                            name: node.base.base.name.clone(),
                            revision: node.active_revision.uid.clone(),
                            size: node
                                .active_revision
                                .claimed_size
                                .unwrap_or(node.total_size_on_cloud_storage)
                                .max(0) as u64,
                            relative_dir: photo_dir,
                            from_photos: true,
                            record_failure: false,
                        });
                    }
                    PotentialObject::Node(node) => self.record_issue(
                        node.uid(),
                        relative_dir,
                        "timeline entry is not a photo file",
                    )?,
                    PotentialObject::Degraded(node) => self.record_issue(
                        node.uid(),
                        relative_dir,
                        "photo metadata could not be decrypted",
                    )?,
                }
            }
            match next_cursor {
                None => break,
                Some(next) => {
                    anyhow::ensure!(
                        cursor.as_ref() != Some(&next),
                        "Photos timeline did not advance"
                    );
                    cursor = Some(next);
                }
            }
        }
        Ok(())
    }

    async fn list_devices(&mut self, drive: &ProtonDriveClient) -> anyhow::Result<()> {
        let relative_dir = Path::new("Computers");
        std::fs::create_dir_all(self.destination.join(relative_dir))?;
        for device in DeviceOperations::list_devices(drive).await? {
            let device_dir = relative_dir.join(safe_name(&device.name));
            std::fs::create_dir_all(self.destination.join(&device_dir))?;
            if let Err(error) = self
                .list_folder(drive, device.root_uid.clone(), &device_dir, true)
                .await
            {
                self.record_issue(&device.root_uid, &device_dir, &error.to_string())?;
            }
        }
        Ok(())
    }

    fn record_issue(&mut self, uid: &NodeUid, path: &Path, error: &str) -> anyhow::Result<()> {
        self.stats.unsupported += 1;
        self.manifest.issues.push(ManifestIssue {
            uid: uid.raw(),
            path: path.to_string_lossy().into_owned(),
            error: error.to_string(),
        });
        save_manifest(&self.manifest_path, &self.manifest)
    }

    /// Downloads every listed file, smallest first.
    async fn download_all(
        &mut self,
        drive: &ProtonDriveClient,
        photos_drive: &ProtonDriveClient,
    ) -> anyhow::Result<()> {
        let mut plan = std::mem::take(&mut self.plan);
        // Stable, so equal sizes keep their listing order.
        plan.sort_by_key(|file| file.size);
        self.progress.status(None);
        self.progress
            .set_totals(plan.len(), plan.iter().map(|file| file.size).sum());
        for (id, file) in plan.iter().enumerate() {
            self.progress.start(
                id,
                file.relative_dir.join(&file.name).display().to_string(),
                file.size,
            );
            let drive = if file.from_photos {
                photos_drive
            } else {
                drive
            };
            match self.export_file(drive, id, file).await {
                Ok(true) => self.progress.finish(id, Outcome::Transferred),
                Ok(false) => self.progress.finish(id, Outcome::Skipped),
                Err(error) if file.record_failure => {
                    self.progress.finish(id, Outcome::Failed);
                    self.progress.warn(&format!(
                        "could not export {}: {error:#}",
                        file.relative_dir.join(&file.name).display()
                    ));
                    self.record_issue(&file.uid, &file.relative_dir, &format!("{error:#}"))?;
                }
                Err(error) => {
                    self.progress.finish(id, Outcome::Failed);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Returns false when the manifest shows the file is already exported.
    async fn export_file(
        &mut self,
        drive: &ProtonDriveClient,
        id: usize,
        file: &PlannedFile,
    ) -> anyhow::Result<bool> {
        let (destination, manifest, direct_output) =
            (&self.destination, &self.manifest, self.direct_output);
        let size = file.size;
        let key = file.uid.raw();
        let mut relative = manifest
            .entries
            .get(&key)
            .and_then(|entry| safe_manifest_path(&entry.path))
            .filter(|path| !direct_output || path.starts_with(&file.relative_dir))
            .unwrap_or_else(|| {
                unique_relative_path(
                    &file.relative_dir,
                    &safe_name(&file.name),
                    destination,
                    manifest,
                )
            });
        let mut output = destination.join(&relative);

        if let Some(entry) = manifest.entries.get(&key) {
            if entry.revision == file.revision.to_string()
                && entry.size == size
                && output.is_file()
                && std::fs::metadata(&output)
                    .map(|meta| meta.len() == size)
                    .unwrap_or(false)
            {
                self.stats.skipped += 1;
                return Ok(false);
            }
        }

        if direct_output && output.exists() {
            relative = unique_relative_path(
                &file.relative_dir,
                &safe_name(&file.name),
                destination,
                manifest,
            );
            output = destination.join(&relative);
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let (download_path, local_file) = if direct_output {
            (
                output.clone(),
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&output)?,
            )
        } else {
            let partial_name = format!(
                ".{}.pdcli-partial",
                output
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("file")
            );
            (0..10_000)
                .find_map(|index| {
                    let path = output.with_file_name(if index == 0 {
                        partial_name.clone()
                    } else {
                        format!("{partial_name}.{index}")
                    });
                    match std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                    {
                        Ok(file) => Some(Ok((path, file))),
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                        Err(error) => Some(Err(error)),
                    }
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("no available partial path for {}", output.display())
                })??
        };
        self.progress.stage(id, Stage::Downloading);
        let download = async {
            let downloader = drive.get_file_downloader(file.revision.clone()).await?;
            downloader
                .download_to_stream(Box::new(local_file), self.progress.callback(id))
                .completion
                .await??;
            Ok::<_, anyhow::Error>(())
        };
        if let Err(error) = download.await {
            let _ = std::fs::remove_file(&download_path);
            return Err(error);
        }
        if size != 0 && std::fs::metadata(&download_path)?.len() != size {
            let actual = std::fs::metadata(&download_path)?.len();
            let _ = std::fs::remove_file(&download_path);
            anyhow::bail!(
                "downloaded {} with {} bytes; expected {}",
                relative.display(),
                actual,
                size
            );
        }
        if !direct_output {
            std::fs::rename(&download_path, &output)?;
        }
        self.manifest.entries.insert(
            key,
            ManifestEntry {
                path: relative.to_string_lossy().into_owned(),
                revision: file.revision.to_string(),
                size,
            },
        );
        save_manifest(&self.manifest_path, &self.manifest)?;
        self.stats.exported += 1;
        self.progress
            .log(&format!("exported {}", relative.display()));
        Ok(true)
    }
}

fn unsupported_media_type(media_type: &str) -> bool {
    is_proton_document(Some(media_type)) || is_proton_sheet(Some(media_type))
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

    #[test]
    fn proton_documents_are_reported_as_unsupported() {
        assert!(unsupported_media_type("application/vnd.proton.doc"));
        assert!(unsupported_media_type("application/vnd.proton.sheet"));
        assert!(!unsupported_media_type("image/jpeg"));
    }

    #[test]
    fn export_name_does_not_overwrite_existing_file() {
        let destination = Path::new(env!("CARGO_MANIFEST_DIR"));
        let relative = unique_relative_path(
            Path::new(""),
            "Cargo.toml",
            destination,
            &Manifest::default(),
        );
        assert_eq!(relative, Path::new("Cargo (1).toml"));
    }
}
