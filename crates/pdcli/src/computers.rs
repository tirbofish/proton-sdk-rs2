use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use proton_drive_sdk::api::devices::DeviceType;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::device_ops::Device;
use proton_drive_sdk::futures::StreamExt;
use proton_drive_sdk::node::revision::RevisionUid;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::proton_sdk_rs2::session::ProtonAPISession;
use proton_drive_sdk::utils::PotentialObject;
use serde::{Deserialize, Serialize};

use crate::{credentials, daemon, flags, fs};

const STATE_FILE: &str = "computers.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncJob {
    pub id: String,
    pub name: String,
    pub local_path: String,
    pub remote_uid: String,
    pub device_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComputersState {
    pub device_id: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub jobs: Vec<SyncJob>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub this_device_id: Option<String>,
    pub this_name: Option<String>,
    pub computers: Vec<Device>,
    pub jobs: Vec<SyncJob>,
}

impl ComputersState {
    fn add_job(&mut self, job: SyncJob) -> anyhow::Result<()> {
        if self
            .jobs
            .iter()
            .any(|existing| existing.local_path == job.local_path)
        {
            anyhow::bail!("already syncing {}", job.local_path);
        }
        self.jobs.push(job);
        Ok(())
    }

    fn remove_job(&mut self, id_or_name: &str) -> bool {
        let before = self.jobs.len();
        self.jobs.retain(|job| {
            job.id != id_or_name && job.name != id_or_name && job.local_path != id_or_name
        });
        self.jobs.len() != before
    }
}

pub fn state_path() -> PathBuf {
    platform_dirs::AppDirs::new(Some("pdcli"), false)
        .map(|dirs| dirs.config_dir.join(STATE_FILE))
        .unwrap_or_else(|| PathBuf::from(STATE_FILE))
}

pub fn load_state() -> ComputersState {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

fn save_state(state: &ComputersState) -> anyhow::Result<()> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    credentials::restrict_permissions(&tmp, 0o600);
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub fn hostname() -> String {
    let mut buf = vec![0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        if let Some(end) = buf.iter().position(|&b| b == 0) {
            buf.truncate(end);
        }
        if let Ok(name) = String::from_utf8(buf) {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    "Linux".into()
}

fn this_device_type() -> DeviceType {
    #[cfg(target_os = "windows")]
    {
        DeviceType::Windows
    }
    #[cfg(target_os = "macos")]
    {
        DeviceType::MacOS
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        DeviceType::Linux
    }
}

pub fn expand_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

pub fn split_remote_path(path: &str) -> Vec<&str> {
    path.split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

pub fn fuse_device_name(name: &str, device_id: &str, taken: &HashSet<String>) -> String {
    let mut base = name.replace(['/', '\0'], "_");
    if base.is_empty() {
        base = "Computer".into();
    }
    if !taken.contains(&base) {
        return base;
    }
    let suffix = if device_id.len() > 8 {
        &device_id[device_id.len() - 8..]
    } else {
        device_id
    };
    format!("{base} ({suffix})")
}

pub async fn open_drive(session: &ProtonAPISession) -> anyhow::Result<ProtonDriveClient> {
    ProtonDriveClient::new(session, None)
}

pub async fn run_cli(
    force_offline: bool,
    command: Option<flags::ComputersCommand>,
    json: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !json || command.is_none(),
        "--json only applies to the computer list"
    );
    if let Some(flags::ComputersCommand::Sync {
        path,
        name,
        dry_run: true,
    }) = &command
    {
        let local_path = expand_path(&path.to_string_lossy());
        validate_sync_path(&local_path)?;
        let local_path = std::fs::canonicalize(&local_path).unwrap_or(local_path);
        let folder_name = name.clone().unwrap_or_else(|| {
            local_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Backup")
                .to_string()
        });
        println!(
            "dry-run: would sync {} -> {} (conflicts become timestamped copies)",
            local_path.display(),
            folder_name
        );
        return Ok(());
    }

    let session = daemon::restore_session(force_offline).await?;
    let drive = open_drive(&session).await?;
    match command {
        None => {
            let snap = snapshot(&drive).await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "this_device_id": snap.this_device_id,
                        "computers": snap.computers.iter().map(|device| serde_json::json!({
                            "id": device.device_id,
                            "name": device.name,
                            "last_sync_time": device.last_sync_time,
                        })).collect::<Vec<_>>(),
                        "jobs": snap.jobs,
                    })
                );
                return Ok(());
            }
            if snap.computers.is_empty() {
                println!("no computers registered");
            }
            for device in &snap.computers {
                let marker = if snap.this_device_id.as_deref() == Some(device.device_id.as_str()) {
                    "*"
                } else {
                    " "
                };
                println!("{marker} {}  {}", device.name, device.device_id);
                for job in snap
                    .jobs
                    .iter()
                    .filter(|job| job.device_id == device.device_id)
                {
                    println!("    {}  ->  {}", job.name, job.local_path);
                }
            }
            for job in snap
                .jobs
                .iter()
                .filter(|job| snap.this_device_id.as_deref() != Some(job.device_id.as_str()))
            {
                if snap
                    .computers
                    .iter()
                    .any(|device| device.device_id == job.device_id)
                {
                    continue;
                }
                println!(
                    "  job {} -> {} ({})",
                    job.name, job.local_path, job.device_id
                );
            }
        }
        Some(flags::ComputersCommand::Register { name, bind }) => {
            let device = register(&drive, name, bind).await?;
            println!("registered as {} ({})", device.name, device.device_id);
        }
        Some(flags::ComputersCommand::Sync {
            path,
            name,
            dry_run,
        }) => {
            let local_path = expand_path(&path.to_string_lossy());
            validate_sync_path(&local_path)?;
            anyhow::ensure!(!dry_run, "dry-run was already handled");
            let job = add_sync(&drive, local_path, name).await?;
            finish_job(&drive, &job).await?;
            println!("syncing {} -> {}", job.local_path, job.name);
        }
        Some(flags::ComputersCommand::Restore {
            computer,
            folder,
            path,
        }) => {
            let job = restore(
                &drive,
                &computer,
                &folder,
                expand_path(&path.to_string_lossy()),
            )
            .await?;
            finish_job(&drive, &job).await?;
            println!(
                "restoring {} from {} to {}",
                job.name, computer, job.local_path
            );
        }
        Some(flags::ComputersCommand::Unsync { job }) => {
            if unsync(&job)? {
                println!("removed sync job {job}");
            } else {
                anyhow::bail!("no sync job named '{job}'");
            }
        }
    }
    Ok(())
}

async fn finish_job(drive: &ProtonDriveClient, job: &SyncJob) -> anyhow::Result<()> {
    if daemon::is_running() {
        daemon::request_retry_sync_now().ok();
        Ok(())
    } else {
        sync_job(drive, job).await
    }
}

pub async fn snapshot(drive: &ProtonDriveClient) -> anyhow::Result<Snapshot> {
    let state = load_state();
    let computers = drive.list_devices().await?;
    Ok(Snapshot {
        this_device_id: state.device_id,
        this_name: state.name,
        computers,
        jobs: state.jobs,
    })
}

pub async fn register(
    drive: &ProtonDriveClient,
    name: Option<String>,
    bind: Option<String>,
) -> anyhow::Result<Device> {
    let mut state = load_state();
    let devices = drive.list_devices().await?;
    let name = name.unwrap_or_else(hostname);

    let device = if let Some(id) = bind {
        devices
            .into_iter()
            .find(|device| device.device_id == id)
            .ok_or_else(|| anyhow::anyhow!("computer '{id}' not found"))?
    } else if let Some(existing) = state
        .device_id
        .as_ref()
        .and_then(|id| devices.iter().find(|device| device.device_id == *id))
    {
        existing.clone()
    } else if let Some(existing) = devices.iter().find(|device| device.name == name) {
        existing.clone()
    } else {
        drive
            .create_device(name.clone(), this_device_type())
            .await?
    };

    state.device_id = Some(device.device_id.clone());
    state.name = Some(device.name.clone());
    save_state(&state)?;
    Ok(device)
}

pub async fn add_sync(
    drive: &ProtonDriveClient,
    local_path: PathBuf,
    name: Option<String>,
) -> anyhow::Result<SyncJob> {
    let local_path = std::fs::canonicalize(&local_path).unwrap_or(local_path);
    validate_sync_path(&local_path)?;

    let device = register(drive, None, None).await?;
    let folder_name = name.unwrap_or_else(|| {
        local_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Backup")
            .to_string()
    });

    let remote = ensure_child_folder(drive, device.root_uid.clone(), &folder_name).await?;
    let job = SyncJob {
        id: job_id(&folder_name),
        name: folder_name,
        local_path: local_path.to_string_lossy().into_owned(),
        remote_uid: remote.raw(),
        device_id: device.device_id,
    };
    let mut state = load_state();
    state.add_job(job.clone())?;
    save_state(&state)?;
    Ok(job)
}

pub async fn restore(
    drive: &ProtonDriveClient,
    computer: &str,
    folder: &str,
    local_path: PathBuf,
) -> anyhow::Result<SyncJob> {
    reject_mount_path(&local_path)?;
    std::fs::create_dir_all(&local_path)?;
    let local_path = std::fs::canonicalize(&local_path).unwrap_or(local_path);

    let devices = drive.list_devices().await?;
    let device = devices
        .into_iter()
        .find(|device| device.device_id == computer || device.name.eq_ignore_ascii_case(computer))
        .ok_or_else(|| anyhow::anyhow!("computer '{computer}' not found"))?;

    let remote = resolve_remote_folder(drive, device.root_uid.clone(), folder).await?;
    let name = split_remote_path(folder)
        .last()
        .copied()
        .unwrap_or(folder)
        .to_string();
    let job = SyncJob {
        id: job_id(&name),
        name,
        local_path: local_path.to_string_lossy().into_owned(),
        remote_uid: remote.raw(),
        device_id: device.device_id,
    };
    let mut state = load_state();
    state.add_job(job.clone())?;
    save_state(&state)?;
    Ok(job)
}

pub fn unsync(id_or_name: &str) -> anyhow::Result<bool> {
    let mut state = load_state();
    let removed = state.remove_job(id_or_name);
    if removed {
        save_state(&state)?;
    }
    Ok(removed)
}

pub async fn sync_job(drive: &ProtonDriveClient, job: &SyncJob) -> anyhow::Result<()> {
    let local = PathBuf::from(&job.local_path);
    std::fs::create_dir_all(&local)?;
    let remote = NodeUid::parse(&job.remote_uid).map_err(|e| anyhow::anyhow!(e))?;
    Box::pin(sync_tree(drive, &local, remote)).await
}

pub async fn sync_loop(drive: ProtonDriveClient) {
    loop {
        if fs::is_online() && !fs::is_sync_paused() {
            let jobs = load_state().jobs;
            for job in jobs {
                if let Err(e) = sync_job(&drive, &job).await {
                    tracing::warn!(
                        job = %job.name,
                        path = %job.local_path,
                        error = %e,
                        "computer sync failed"
                    );
                }
            }
        }
        tokio::select! {
            _ = fs::sync_notify().notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
    }
}

fn job_id(name: &str) -> String {
    format!(
        "{}-{}",
        name.replace(['/', ' '], "-"),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    )
}

fn reject_mount_path(path: &Path) -> anyhow::Result<()> {
    if let Ok(mount) = fs::default_mountpoint() {
        if path.starts_with(&mount) || mount.starts_with(path) {
            anyhow::bail!("cannot sync the Proton Drive mount itself");
        }
    }
    Ok(())
}

fn validate_sync_path(path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(path.is_dir(), "{} is not a directory", path.display());
    reject_mount_path(path)
}

async fn resolve_remote_folder(
    drive: &ProtonDriveClient,
    mut current: NodeUid,
    path: &str,
) -> anyhow::Result<NodeUid> {
    for part in split_remote_path(path) {
        current = child_folder(drive, current, part)
            .await?
            .ok_or_else(|| anyhow::anyhow!("folder '{part}' not found"))?;
    }
    Ok(current)
}

async fn ensure_child_folder(
    drive: &ProtonDriveClient,
    parent: NodeUid,
    name: &str,
) -> anyhow::Result<NodeUid> {
    if let Some(existing) = child_folder(drive, parent.clone(), name).await? {
        return Ok(existing);
    }
    Ok(drive
        .create_folder(parent, name.to_string(), None)
        .await?
        .base
        .uid)
}

async fn child_folder(
    drive: &ProtonDriveClient,
    parent: NodeUid,
    name: &str,
) -> anyhow::Result<Option<NodeUid>> {
    let children = list_children(drive, parent).await?;
    Ok(children.into_iter().find_map(|child| {
        if child.is_dir && child.name == name {
            Some(child.uid)
        } else {
            None
        }
    }))
}

struct RemoteChild {
    name: String,
    uid: NodeUid,
    is_dir: bool,
    mtime: i64,
    revision: Option<RevisionUid>,
}

async fn list_children(
    drive: &ProtonDriveClient,
    parent: NodeUid,
) -> anyhow::Result<Vec<RemoteChild>> {
    let stream = drive.enumerate_folder_children(parent).await?;
    tokio::pin!(stream);
    let mut children = Vec::new();
    while let Some(item) = stream.next().await {
        match item? {
            PotentialObject::Node(Node::Folder(folder) | Node::Album(folder)) => {
                if folder.base.trash_time.is_some() {
                    continue;
                }
                children.push(RemoteChild {
                    name: folder.base.name.clone(),
                    uid: folder.base.uid,
                    is_dir: true,
                    mtime: folder.base.creation_time.timestamp(),
                    revision: None,
                });
            }
            PotentialObject::Node(Node::File(file) | Node::Photo(file)) => {
                if file.base.base.trash_time.is_some() {
                    continue;
                }
                children.push(RemoteChild {
                    name: file.base.base.name.clone(),
                    uid: file.base.base.uid,
                    is_dir: false,
                    mtime: file
                        .active_revision
                        .claimed_modification_time
                        .map(|t| t.timestamp())
                        .unwrap_or_else(|| file.base.base.creation_time.timestamp()),
                    revision: Some(file.active_revision.uid.clone()),
                });
            }
            _ => {}
        }
    }
    Ok(children)
}

async fn sync_tree(drive: &ProtonDriveClient, local: &Path, remote: NodeUid) -> anyhow::Result<()> {
    let remote_children = list_children(drive, remote.clone()).await?;
    let by_name: HashMap<String, &RemoteChild> = remote_children
        .iter()
        .map(|child| (child.name.clone(), child))
        .collect();
    let mut seen = HashSet::new();

    let entries = match std::fs::read_dir(local) {
        Ok(entries) => entries,
        Err(e) => anyhow::bail!("read {}: {e}", local.display()),
    };

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Conflict copies are recovery artifacts, not new backup inputs.
        if is_conflict_name(name) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        seen.insert(name.to_string());
        let path = entry.path();
        if file_type.is_dir() {
            let child = match by_name.get(name) {
                Some(child) if child.is_dir => child.uid.clone(),
                Some(_) => {
                    tracing::warn!(path = %path.display(), "skipping local folder; remote is a file");
                    continue;
                }
                None => {
                    drive
                        .create_folder(remote.clone(), name.to_string(), None)
                        .await?
                        .base
                        .uid
                }
            };
            Box::pin(sync_tree(drive, &path, child)).await?;
        } else if file_type.is_file() {
            match by_name.get(name) {
                Some(child) if child.is_dir => {
                    tracing::warn!(path = %path.display(), "skipping local file; remote is a folder");
                }
                Some(child) => {
                    let local_mtime = file_mtime(&path);
                    if local_mtime > child.mtime {
                        let conflict = copy_conflict_file(&path, "remote")?;
                        download_file(drive, child, &conflict).await?;
                        tracing::warn!(
                            path = %path.display(),
                            conflict = %conflict.display(),
                            "local and remote changed; preserved the remote copy"
                        );
                        upload_revision(drive, child, &path).await?;
                    } else if child.mtime > local_mtime {
                        let conflict = copy_conflict_file(&path, "local")?;
                        tracing::warn!(
                            path = %path.display(),
                            conflict = %conflict.display(),
                            "local and remote changed; preserved the local copy"
                        );
                        download_file(drive, child, &path).await?;
                    }
                }
                None => upload_new(drive, remote.clone(), &path, name).await?,
            }
        }
    }

    // ponytail: no deletes. add a recycle confirm if mirroring is required.
    for child in &remote_children {
        if seen.contains(&child.name) {
            continue;
        }
        let dest = local.join(&child.name);
        if child.is_dir {
            std::fs::create_dir_all(&dest)?;
            Box::pin(sync_tree(drive, &dest, child.uid.clone())).await?;
        } else {
            download_file(drive, child, &dest).await?;
        }
    }
    Ok(())
}

fn is_conflict_name(name: &str) -> bool {
    name.contains(" (pdcli conflict ")
}

fn copy_conflict_file(path: &Path, side: &str) -> anyhow::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("{} is not a valid UTF-8 filename", path.display()))?;
    let (stem, extension) = file_name
        .rsplit_once('.')
        .filter(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
        .map_or((file_name, ""), |parts| parts);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    for index in 0..1000 {
        let suffix = if extension.is_empty() {
            String::new()
        } else {
            format!(".{extension}")
        };
        let candidate = parent.join(format!(
            "{stem} (pdcli conflict {side} {stamp}{index}){suffix}"
        ));
        if !candidate.exists() {
            std::fs::copy(path, &candidate)?;
            return Ok(candidate);
        }
    }
    anyhow::bail!("could not create a conflict copy for {}", path.display())
}

fn file_mtime(path: &Path) -> i64 {
    path.metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn upload_new(
    drive: &ProtonDriveClient,
    parent: NodeUid,
    path: &Path,
    name: &str,
) -> anyhow::Result<()> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len() as i64;
    let last_mod = meta.modified().ok();
    let media_type = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    let uploader = drive
        .get_file_uploader(
            parent,
            name.to_string(),
            media_type,
            size,
            last_mod,
            None,
            None,
            true,
        )
        .await?;
    let file = tokio::fs::File::open(path).await?;
    let reader: Box<dyn tokio::io::AsyncRead + Unpin + Send> = Box::new(file);
    uploader
        .upload_from_stream(reader, vec![], Box::new(|_, _| {}))
        .await?;
    tracing::info!(path = %path.display(), "uploaded to computer backup");
    Ok(())
}

async fn upload_revision(
    drive: &ProtonDriveClient,
    child: &RemoteChild,
    path: &Path,
) -> anyhow::Result<()> {
    let Some(revision) = child.revision.clone() else {
        tracing::warn!(path = %path.display(), "remote file has no revision; skip");
        return Ok(());
    };
    let meta = std::fs::metadata(path)?;
    let uploader = drive
        .get_file_revision_uploader(
            revision,
            meta.len() as i64,
            meta.modified().ok(),
            None,
            None,
        )
        .await?;
    let file = tokio::fs::File::open(path).await?;
    let reader: Box<dyn tokio::io::AsyncRead + Unpin + Send> = Box::new(file);
    uploader
        .upload_from_stream(reader, vec![], Box::new(|_, _| {}))
        .await?;
    tracing::info!(path = %path.display(), "updated computer backup");
    Ok(())
}

async fn download_file(
    drive: &ProtonDriveClient,
    child: &RemoteChild,
    path: &Path,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    drive
        .download_to_file(child.uid.clone(), path, Box::new(|_, _| {}))
        .await?;
    if child.mtime > 0 {
        let time = filetime::FileTime::from_unix_time(child.mtime, 0);
        let _ = filetime::set_file_mtime(path, time);
    }
    tracing::info!(path = %path.display(), "restored from computer backup");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_fuse_names() {
        let mut taken = HashSet::new();
        let first = fuse_device_name("Office PC", "abcdef12", &taken);
        taken.insert(first.clone());
        let second = fuse_device_name("Office PC", "zzz99999", &taken);
        assert_eq!(first, "Office PC");
        assert_eq!(second, "Office PC (zzz99999)");
        assert_eq!(fuse_device_name("a/b", "id", &HashSet::new()), "a_b");
    }

    #[test]
    fn remote_path_splits() {
        assert!(split_remote_path("").is_empty());
        assert!(split_remote_path(".").is_empty());
        assert_eq!(split_remote_path("Documents"), vec!["Documents"]);
        assert_eq!(
            split_remote_path("/Documents/Work/"),
            vec!["Documents", "Work"]
        );
    }

    #[test]
    fn job_local_path_is_unique() {
        let mut state = ComputersState::default();
        let job = |path: &str| SyncJob {
            id: path.into(),
            name: "Documents".into(),
            local_path: path.into(),
            remote_uid: "vol~link".into(),
            device_id: "dev".into(),
        };
        state.add_job(job("/tmp/a")).unwrap();
        assert!(state.add_job(job("/tmp/a")).is_err());
        assert!(state.remove_job("Documents"));
        assert!(state.jobs.is_empty());
    }

    #[test]
    fn conflict_copies_are_not_resynced() {
        assert!(is_conflict_name("notes (pdcli conflict local 1230).txt"));
        assert!(!is_conflict_name("notes.txt"));
    }
}
