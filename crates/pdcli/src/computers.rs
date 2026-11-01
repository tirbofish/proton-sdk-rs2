use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

use crate::transfer_progress::{Labels, Outcome, Stage, TransferProgress};
use crate::{credentials, daemon, flags, fs};

const SYNC_LABELS: Labels = Labels {
    verb: "Syncing",
    noun: "files",
    done: "transferred",
};

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

    fn forget_device(&mut self, device_id: &str) {
        self.jobs.retain(|job| job.device_id != device_id);
        if self.device_id.as_deref() == Some(device_id) {
            self.device_id = None;
            self.name = None;
        }
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
        Some(flags::ComputersCommand::Remove { computer, yes }) => {
            let device = find_device(&drive, &computer).await?;
            if !yes && !confirm_remove(&device.name)? {
                anyhow::bail!("cancelled");
            }
            remove(&drive, &device).await?;
            println!("removed {} and its backups from Drive", device.name);
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
        let progress = Arc::new(TransferProgress::new(SYNC_LABELS));
        progress.drive(sync_job(drive, job, &progress)).await
    }
}

pub async fn snapshot(drive: &ProtonDriveClient) -> anyhow::Result<Snapshot> {
    let state = load_state();
    let computers = drive.list_devices().await?;
    Ok(Snapshot {
        this_device_id: state.device_id,
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

    let device = find_device(drive, computer).await?;

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

async fn find_device(drive: &ProtonDriveClient, computer: &str) -> anyhow::Result<Device> {
    let devices = drive.list_devices().await?;
    if let Some(device) = devices.iter().find(|device| device.device_id == computer) {
        return Ok(device.clone());
    }
    let mut named = devices
        .into_iter()
        .filter(|device| device.name.eq_ignore_ascii_case(computer));
    match (named.next(), named.next()) {
        (Some(device), None) => Ok(device),
        (Some(_), Some(_)) => {
            anyhow::bail!("more than one computer is named '{computer}'; use its ID instead")
        }
        _ => anyhow::bail!("computer '{computer}' not found"),
    }
}

fn confirm_remove(name: &str) -> anyhow::Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    anyhow::ensure!(
        std::io::stdin().is_terminal(),
        "pass --yes to remove '{name}' without a prompt"
    );
    eprint!("Delete '{name}' and all of its backups from Drive? This cannot be undone. [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// Deletes the computer from Drive, then forgets every local job that synced to it.
pub async fn remove(drive: &ProtonDriveClient, device: &Device) -> anyhow::Result<()> {
    drive.delete_device(&device.device_id).await?;
    let mut state = load_state();
    state.forget_device(&device.device_id);
    save_state(&state)
}

pub fn unsync(id_or_name: &str) -> anyhow::Result<bool> {
    let mut state = load_state();
    let removed = state.remove_job(id_or_name);
    if removed {
        save_state(&state)?;
    }
    Ok(removed)
}

pub async fn sync_job(
    drive: &ProtonDriveClient,
    job: &SyncJob,
    progress: &Arc<TransferProgress>,
) -> anyhow::Result<()> {
    let local = PathBuf::from(&job.local_path);
    std::fs::create_dir_all(&local)?;
    let remote = NodeUid::parse(&job.remote_uid).map_err(|e| anyhow::anyhow!(e))?;
    let mut plan = Vec::new();
    Box::pin(plan_tree(drive, &local, remote, &mut plan, progress)).await?;
    run_plan(drive, plan, progress).await
}

pub async fn sync_loop(drive: ProtonDriveClient) {
    loop {
        if fs::is_online() && !fs::is_sync_paused() {
            let jobs = load_state().jobs;
            for job in jobs {
                let progress = Arc::new(TransferProgress::hidden(SYNC_LABELS));
                if let Err(e) = sync_job(&drive, &job, &progress).await {
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

#[derive(Clone)]
struct RemoteChild {
    name: String,
    uid: NodeUid,
    is_dir: bool,
    mtime: i64,
    size: u64,
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
                    size: 0,
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
                    size: file
                        .active_revision
                        .claimed_size
                        .unwrap_or(file.total_size_on_cloud_storage)
                        .max(0) as u64,
                    revision: Some(file.active_revision.uid.clone()),
                });
            }
            _ => {}
        }
    }
    Ok(children)
}

/// A file transfer found while comparing a local folder with its backup.
enum SyncAction {
    Upload {
        parent: NodeUid,
        path: PathBuf,
        name: String,
    },
    /// Both sides changed and the local copy is newer: keep the remote one as a conflict copy.
    ReplaceRemote {
        child: RemoteChild,
        path: PathBuf,
    },
    /// Both sides changed and the remote copy is newer: keep the local one as a conflict copy.
    ReplaceLocal {
        child: RemoteChild,
        path: PathBuf,
    },
    Download {
        child: RemoteChild,
        path: PathBuf,
    },
}

struct PlannedSync {
    action: SyncAction,
    // Every byte this action moves, used to order and measure the sync.
    size: u64,
}

impl PlannedSync {
    fn path(&self) -> &Path {
        match &self.action {
            SyncAction::Upload { path, .. }
            | SyncAction::ReplaceRemote { path, .. }
            | SyncAction::ReplaceLocal { path, .. }
            | SyncAction::Download { path, .. } => path,
        }
    }
}

/// Creates missing folders on both sides and lists the file transfers still needed.
async fn plan_tree(
    drive: &ProtonDriveClient,
    local: &Path,
    remote: NodeUid,
    plan: &mut Vec<PlannedSync>,
    progress: &TransferProgress,
) -> anyhow::Result<()> {
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
                    progress.warn(&format!(
                        "skipping local folder {}; remote is a file",
                        path.display()
                    ));
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
            Box::pin(plan_tree(drive, &path, child, plan, progress)).await?;
        } else if file_type.is_file() {
            let local_size = entry.metadata()?.len();
            let planned = match by_name.get(name) {
                Some(child) if child.is_dir => {
                    progress.warn(&format!(
                        "skipping local file {}; remote is a folder",
                        path.display()
                    ));
                    None
                }
                Some(child) => {
                    let local_mtime = file_mtime(&path);
                    if local_mtime > child.mtime {
                        Some(PlannedSync {
                            size: child.size + local_size,
                            action: SyncAction::ReplaceRemote {
                                child: (*child).clone(),
                                path,
                            },
                        })
                    } else if child.mtime > local_mtime {
                        Some(PlannedSync {
                            size: child.size,
                            action: SyncAction::ReplaceLocal {
                                child: (*child).clone(),
                                path,
                            },
                        })
                    } else {
                        None
                    }
                }
                None => Some(PlannedSync {
                    size: local_size,
                    action: SyncAction::Upload {
                        parent: remote.clone(),
                        path,
                        name: name.to_string(),
                    },
                }),
            };
            plan.extend(planned);
        }
        progress.status(Some(format!("comparing: {} transfers found", plan.len())));
    }

    // ponytail: no deletes. add a recycle confirm if mirroring is required.
    for child in &remote_children {
        if seen.contains(&child.name) {
            continue;
        }
        let dest = local.join(&child.name);
        if child.is_dir {
            std::fs::create_dir_all(&dest)?;
            Box::pin(plan_tree(drive, &dest, child.uid.clone(), plan, progress)).await?;
        } else {
            plan.push(PlannedSync {
                size: child.size,
                action: SyncAction::Download {
                    child: child.clone(),
                    path: dest,
                },
            });
        }
    }
    Ok(())
}

/// Runs the planned transfers smallest first, stopping at the first failure.
async fn run_plan(
    drive: &ProtonDriveClient,
    mut plan: Vec<PlannedSync>,
    progress: &Arc<TransferProgress>,
) -> anyhow::Result<()> {
    // Stable, so equal sizes keep their folder order.
    plan.sort_by_key(|item| item.size);
    progress.status(None);
    progress.set_totals(plan.len(), plan.iter().map(|item| item.size).sum());
    for (id, item) in plan.iter().enumerate() {
        progress.start(id, item.path().display().to_string(), item.size);
        let result = match &item.action {
            SyncAction::Upload { parent, path, name } => {
                upload_new(drive, parent.clone(), path, name, progress, id).await
            }
            SyncAction::ReplaceRemote { child, path } => {
                async {
                    let conflict = copy_conflict_file(path, "remote")?;
                    download_file(drive, child, &conflict, progress.callback(id), progress, id)
                        .await?;
                    progress.warn(&format!(
                        "{} changed on both sides; kept the remote copy as {}",
                        path.display(),
                        conflict.display()
                    ));
                    upload_revision(
                        drive,
                        child,
                        path,
                        progress.callback_after(id, child.size),
                        progress,
                        id,
                    )
                    .await
                }
                .await
            }
            SyncAction::ReplaceLocal { child, path } => {
                async {
                    let conflict = copy_conflict_file(path, "local")?;
                    progress.warn(&format!(
                        "{} changed on both sides; kept the local copy as {}",
                        path.display(),
                        conflict.display()
                    ));
                    download_file(drive, child, path, progress.callback(id), progress, id).await
                }
                .await
            }
            SyncAction::Download { child, path } => {
                download_file(drive, child, path, progress.callback(id), progress, id).await
            }
        };
        if let Err(error) = result {
            progress.finish(id, Outcome::Failed);
            return Err(error.context(format!("sync {}", item.path().display())));
        }
        progress.finish(id, Outcome::Transferred);
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
    progress: &Arc<TransferProgress>,
    id: usize,
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
    progress.stage(id, Stage::Uploading);
    uploader
        .upload_from_stream(reader, vec![], progress.callback(id))
        .await?;
    progress.log(&format!("uploaded {}", path.display()));
    Ok(())
}

async fn upload_revision(
    drive: &ProtonDriveClient,
    child: &RemoteChild,
    path: &Path,
    on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    progress: &TransferProgress,
    id: usize,
) -> anyhow::Result<()> {
    let Some(revision) = child.revision.clone() else {
        progress.warn(&format!(
            "remote file {} has no revision; skipped",
            path.display()
        ));
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
    progress.stage(id, Stage::Uploading);
    uploader
        .upload_from_stream(reader, vec![], on_progress)
        .await?;
    progress.log(&format!("updated {}", path.display()));
    Ok(())
}

async fn download_file(
    drive: &ProtonDriveClient,
    child: &RemoteChild,
    path: &Path,
    on_progress: Box<dyn Fn(i64, i64) + Send + Sync>,
    progress: &TransferProgress,
    id: usize,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    progress.stage(id, Stage::Downloading);
    drive
        .download_to_file(child.uid.clone(), path, on_progress)
        .await?;
    if child.mtime > 0 {
        let time = filetime::FileTime::from_unix_time(child.mtime, 0);
        let _ = filetime::set_file_mtime(path, time);
    }
    progress.log(&format!("downloaded {}", path.display()));
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
    fn removing_a_computer_forgets_its_jobs_and_registration() {
        let job = |path: &str, device: &str| SyncJob {
            id: path.into(),
            name: path.into(),
            local_path: path.into(),
            remote_uid: "vol~link".into(),
            device_id: device.into(),
        };
        let mut state = ComputersState {
            device_id: Some("gone".into()),
            name: Some("Old PC".into()),
            jobs: vec![job("/a", "gone"), job("/b", "kept"), job("/c", "gone")],
        };
        state.forget_device("gone");
        assert_eq!(state.device_id, None);
        assert_eq!(state.name, None);
        let remaining: Vec<_> = state
            .jobs
            .iter()
            .map(|job| job.local_path.as_str())
            .collect();
        assert_eq!(remaining, ["/b"]);

        state.device_id = Some("kept".into());
        state.forget_device("other");
        assert_eq!(state.device_id.as_deref(), Some("kept"));
        assert_eq!(state.jobs.len(), 1);
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
