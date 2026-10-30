//! Choosing which files keep a local copy, like OneDrive's "Always keep on this
//! device" and "Free up space". Changes go through the mounted filesystem's
//! `user.pdcli.availability` attribute, so the running daemon does the work.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::NodeUid;
use serde::Serialize;

use crate::db::FuseDb;
use crate::flags::OfflineCommand;
use crate::fs::{AVAILABILITY_XATTR, AVAILABLE_OFFLINE, ONLINE_ONLY};
use crate::{daemon, fs};

/// Name of the My Files folder at the mount root.
const MY_FILES: &str = "MyFiles";

#[derive(Serialize)]
struct OfflineFile {
    uid: Option<String>,
    path: PathBuf,
    size: u64,
}

pub async fn run_cli(force_offline: bool, command: OfflineCommand) -> anyhow::Result<()> {
    match command {
        OfflineCommand::List { json } => list(json),
        OfflineCommand::Keep { targets } => apply(force_offline, &targets, AVAILABLE_OFFLINE).await,
        OfflineCommand::Free { targets } => apply(force_offline, &targets, ONLINE_ONLY).await,
    }
}

fn list(json: bool) -> anyhow::Result<()> {
    let db =
        FuseDb::open_default().context("cannot open the mount's database; mount once first")?;
    let mut files: Vec<OfflineFile> = db
        .cached_file_inodes()
        .into_iter()
        .filter_map(|(ino, cached_path)| {
            let size = std::fs::metadata(&cached_path).ok()?.len();
            let row = db.get_inode(ino)?;
            Some(OfflineFile {
                uid: row.node_uid,
                path: db.path_components(ino)?.iter().collect(),
                size,
            })
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    if json {
        println!("{}", serde_json::to_string(&files)?);
    } else {
        for file in &files {
            println!("{:>12}  {}", file.size, file.path.display());
        }
        let total: u64 = files.iter().map(|file| file.size).sum();
        println!("{} files, {total} bytes available offline", files.len());
    }
    Ok(())
}

async fn apply(force_offline: bool, targets: &[String], value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        daemon::is_running(),
        "the Drive is not mounted; run `pdcli mount` first"
    );
    let mountpoint = fs::default_mountpoint()?;
    let mut drive = None;
    let mut tally = Tally::default();
    for target in targets {
        let path = match resolve(target, &mountpoint, force_offline, &mut drive).await {
            Ok(path) => path,
            Err(error) => {
                eprintln!("{target}: {error:#}");
                tally.failed += 1;
                continue;
            }
        };
        apply_recursive(&path, value, &mut tally);
    }
    let files = if tally.changed == 1 { "file" } else { "files" };
    if value == AVAILABLE_OFFLINE {
        println!("{} {files} available offline", tally.changed);
    } else {
        println!("{} {files} now online-only", tally.changed);
    }
    anyhow::ensure!(
        tally.failed == 0,
        "{} item(s) could not be changed",
        tally.failed
    );
    Ok(())
}

#[derive(Default)]
struct Tally {
    changed: usize,
    failed: usize,
}

/// Accepts a path inside the mount or a node UID from `pdcli browse`.
async fn resolve(
    target: &str,
    mountpoint: &Path,
    force_offline: bool,
    drive: &mut Option<ProtonDriveClient>,
) -> anyhow::Result<PathBuf> {
    let candidate = Path::new(target);
    if candidate.exists() {
        let path = std::path::absolute(candidate)?;
        anyhow::ensure!(
            path.starts_with(mountpoint),
            "not inside the Drive mount at {}",
            mountpoint.display()
        );
        return Ok(path);
    }
    let uid = NodeUid::parse(target)
        .map_err(|error| anyhow::anyhow!("not a path or node UID: {error}"))?;
    // The mount only knows folders that were browsed, so walk up through the
    // API until reaching one it knows, then descend by name.
    let db = FuseDb::open_default()?;
    let mut names = Vec::new();
    let mut current = uid;
    for _ in 0..256 {
        if let Some(row) = db.find_by_node_uid(&current.raw()) {
            let mut path = mountpoint.to_path_buf();
            path.extend(db.path_components(row.ino).context("broken folder chain")?);
            path.extend(names.iter().rev());
            return Ok(path);
        }
        if drive.is_none() {
            let session = daemon::restore_session(force_offline).await?;
            *drive = Some(ProtonDriveClient::new(&session, None)?);
        }
        let node = drive
            .as_ref()
            .expect("client created above")
            .get_node(current.clone())
            .await?
            .result()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let Some(parent) = node.base().parent_uid.clone() else {
            // A share root the mount has not recorded yet: My Files.
            let mut path = mountpoint.join(MY_FILES);
            path.extend(names.iter().rev());
            return Ok(path);
        };
        names.push(node.base().name.clone());
        current = parent;
    }
    anyhow::bail!("folder nesting is too deep")
}

fn apply_recursive(path: &Path, value: &str, tally: &mut Tally) {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("{}: {error}", path.display());
            tally.failed += 1;
            return;
        }
    };
    if metadata.is_dir() {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("{}: {error}", path.display());
                tally.failed += 1;
                return;
            }
        };
        for entry in entries {
            match entry {
                Ok(entry) => apply_recursive(&entry.path(), value, tally),
                Err(error) => {
                    eprintln!("{}: {error}", path.display());
                    tally.failed += 1;
                }
            }
        }
        return;
    }
    if !metadata.is_file() {
        return;
    }
    match set_availability(path, value) {
        Ok(()) => tally.changed += 1,
        // Proton Docs and Sheets exist only online.
        Err(error) if error.raw_os_error() == Some(libc::ENOTSUP) => {
            eprintln!("{}: Proton Docs and Sheets are online-only", path.display());
        }
        Err(error) => {
            let reason = match error.raw_os_error() {
                Some(libc::EBUSY) => "has unsynced changes or is open".to_string(),
                Some(libc::EHOSTUNREACH) => "cannot download while offline".to_string(),
                _ => error.to_string(),
            };
            eprintln!("{}: {reason}", path.display());
            tally.failed += 1;
        }
    }
}

fn set_availability(path: &Path, value: &str) -> std::io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let name = CString::new(AVAILABILITY_XATTR).expect("constant has no NUL");
    let result = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
