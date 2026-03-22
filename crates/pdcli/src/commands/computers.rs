use crate::state::ReplState;
use anyhow::{anyhow, Result};
use futures::StreamExt;
use proton_drive_sdk::api::devices::DeviceType;
use proton_drive_sdk::node::{Node, NodeUid};
use std::sync::Arc;
use parking_lot::Mutex;

use super::helpers::{list_children, new_spinner, upload_progress_bar, progress_callback, finish_progress};

/// Max parallel uploads per folder level.
const UPLOAD_CONCURRENCY: usize = 4;

pub async fn computers_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let sub = args.first().copied().unwrap_or("ls");
    match sub {
        "ls" | "list" => computers_list(&args[args.len().min(1)..], state).await,
        "add" | "create" => computers_add(&args[1..], state).await,
        "rename" | "mv" => computers_rename(&args[1..], state).await,
        "rm" | "delete" | "remove" => computers_rm(&args[1..], state).await,
        "sync" => computers_sync(&args[1..], state).await,
        "help" => {
            print_computers_help();
            Ok(())
        }
        _ => Err(anyhow!(
            "Unknown computers subcommand '{}'. Use 'computers help'.",
            sub
        )),
    }
}

async fn computers_list(_args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let client = {
        let s = state.lock();
        s.get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone()
    };

    let sp = new_spinner("Fetching computers...");
    let devices = client.list_devices().await?;
    sp.finish_and_clear();

    // Keep state.computers fresh so `cd Computers/<name>` works.
    {
        let computers: Vec<_> = devices
            .iter()
            .map(|d| (d.device_id.clone(), d.name.clone(), d.volume_id.clone(), d.root_uid.link_id.clone()))
            .collect();
        state.lock().set_computers(computers);
    }

    if devices.is_empty() {
        println!("\n  No computers registered.\n");
        return Ok(());
    }

    println!();
    println!("  {:30}  {:8}  {:26}  {}", "Name", "Type", "Created", "ID");
    println!("  {}  {}  {}  {}", "-".repeat(30), "-".repeat(8), "-".repeat(26), "-".repeat(36));
    for d in &devices {
        let type_str = device_type_label(d.device_type);
        let created = d.create_time.format("%Y-%m-%d %H:%M UTC").to_string();
        println!(
            "  {:30}  {:8}  {:26}  {}",
            truncate(&d.name, 30),
            type_str,
            created,
            d.device_id,
        );
    }
    println!("\n  {} computer(s)\n", devices.len());
    Ok(())
}

async fn computers_add(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: computers add <name> [windows|macos|linux]"));
    }
    let name = args[0].to_string();
    let device_type = if let Some(t) = args.get(1) {
        parse_device_type(t)?
    } else {
        DeviceType::Linux
    };

    let client = {
        let s = state.lock();
        s.get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone()
    };

    let sp = new_spinner(format!("Registering computer '{}'...", name));
    let device = client.create_device(name.clone(), device_type).await?;
    sp.finish_and_clear();

    println!(
        "Computer '{}' registered (ID: {}).",
        device.name, device.device_id
    );
    Ok(())
}

async fn computers_rename(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.len() < 2 {
        return Err(anyhow!("Usage: computers rename <device_id> <new_name>"));
    }
    let device_id = args[0];
    let new_name = args[1].to_string();

    let client = {
        let s = state.lock();
        s.get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone()
    };

    let sp = new_spinner(format!("Renaming computer '{}'...", device_id));
    let device = client.rename_device(device_id, new_name).await?;
    sp.finish_and_clear();

    println!("Computer renamed to '{}'.", device.name);
    Ok(())
}

async fn computers_rm(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: computers rm <device_id>"));
    }
    let device_id = args[0];

    // Resolve the name for a friendlier confirmation message.
    let (client, name) = {
        let s = state.lock();
        let client = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        (client, device_id.to_string())
    };

    let confirmed = dialoguer::Confirm::new()
        .with_prompt(format!("Unregister computer '{}'?", name))
        .default(false)
        .interact()?;

    if !confirmed {
        println!("Aborted.");
        return Ok(());
    }

    let sp = new_spinner("Unregistering computer...");
    client.delete_device(device_id).await?;
    sp.finish_and_clear();
    println!("Computer '{}' unregistered.", name);
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn device_type_label(t: DeviceType) -> &'static str {
    match t {
        DeviceType::Windows => "Windows",
        DeviceType::MacOS => "macOS",
        DeviceType::Linux => "Linux",
    }
}

fn parse_device_type(s: &str) -> Result<DeviceType> {
    match s.to_lowercase().as_str() {
        "windows" => Ok(DeviceType::Windows),
        "macos" | "mac" => Ok(DeviceType::MacOS),
        "linux" => Ok(DeviceType::Linux),
        other => Err(anyhow!(
            "Unknown device type '{}'. Use windows, macos, or linux.",
            other
        )),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

fn print_computers_help() {
    println!(
        r#"
COMMAND: computers [subcommand]

Manage registered Computers (backup devices) for this account.

SUBCOMMANDS:
  ls                       List all registered computers
  add <name> [type]        Register a new computer (type: windows, macos, linux)
  rename <id> <new_name>   Rename an existing computer
  rm <id>                  Unregister a computer
  sync <local_folder>      Sync a local folder to this computer's Proton Drive backup

EXAMPLES:
  computers ls
  computers add "My Server" linux
  computers rename abc123 "Workstation"
  computers rm abc123
  computers sync ~/Documents
"#
    );
}

// ── Sync ──────────────────────────────────────────────────────────────────────

async fn computers_sync(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: computers sync <local_folder>"));
    }

    let raw_path = args[0].to_string();
    let local_folder = std::path::PathBuf::from(
        if raw_path.starts_with("~/") {
            if let Some(home) = std::env::var_os("HOME") {
                std::path::PathBuf::from(home).join(&raw_path[2..]).to_string_lossy().to_string()
            } else {
                raw_path.clone()
            }
        } else {
            raw_path.clone()
        }
    );

    if !local_folder.exists() {
        return Err(anyhow!("Folder '{}' does not exist.", local_folder.display()));
    }
    if !local_folder.is_dir() {
        return Err(anyhow!("'{}' is not a directory.", local_folder.display()));
    }

    let hostname = get_hostname();
    let folder_name = local_folder.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "backup".to_string());

    println!("  Syncing '{}' → Computers/{}/{}", local_folder.display(), hostname, folder_name);

    let (client, cache) = {
        let s = state.lock();
        let client = s.get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let cache = s.get_cache();
        (client, cache)
    };
    let sp = new_spinner(format!("Looking up computer '{}'...", hostname));
    let devices = client.list_devices().await?;
    sp.finish_and_clear();

    let (device, is_new_device) = if let Some(d) = devices.into_iter().find(|d| d.name == hostname) {
        println!("  Using existing computer: {} ({})", d.name, d.device_id);
        (d, false)
    } else {
        let sp = new_spinner(format!("Registering this computer as '{}'...", hostname));
        let d = client.create_device(hostname.clone(), DeviceType::Linux).await?;
        sp.finish_and_clear();
        println!("  Registered new computer: {} ({})", d.name, d.device_id);
        (d, true)
    };

    let existing_remote: Vec<Node> = if is_new_device {
        Vec::new()
    } else {
        let sp = new_spinner("Fetching remote file list...");
        let result = list_children(&client, device.root_uid.clone()).await?;
        sp.finish_and_clear();
        result
    };

    // The API forbids creating files at the device root.  We need a subfolder
    // named after the synced directory (e.g. "Screenshots").
    let sync_folder_uid = if let Some(n) = existing_remote.iter().find(|n| n.base().name == folder_name) {
        n.uid().clone()
    } else {
        let sp = new_spinner(format!("Creating remote folder '{}'...", folder_name));
        let f = client.create_folder(device.root_uid.clone(), folder_name.clone(), None).await
            .map_err(|e| { sp.finish_and_clear(); e })?;
        sp.finish_and_clear();
        f.base.uid
    };

    // Fetch children of the sync folder (the actual upload target).
    let sync_folder_children: Vec<Node> = {
        let sp = new_spinner("Fetching existing files...");
        let result = list_children(&client, sync_folder_uid.clone()).await?;
        sp.finish_and_clear();
        result
    };

    // Persist the sync config so it is auto-resumed on the next mount.
    if let Some(ref c) = cache {
        if let Err(e) = c.save_computer_sync_config(&device.device_id, &local_folder) {
            eprintln!("  [warn] Could not save sync config: {e}");
        }
    }

    // Run sync inline (blocking) so the REPL waits until completion.
    let remote_names: std::collections::HashSet<String> = sync_folder_children
        .iter()
        .map(|n| n.base().name.clone())
        .collect();

    let (mut uploaded, mut skipped, mut errors) = (0usize, 0usize, 0usize);

    // Collect files and dirs at the root level.
    let entries: Vec<_> = match std::fs::read_dir(&local_folder) {
        Ok(it) => it.flatten().collect(),
        Err(e) => return Err(anyhow!("Cannot read '{}': {e}", local_folder.display())),
    };

    let files: Vec<_> = entries.iter()
        .filter(|e| e.path().is_file())
        .map(|e| e.path())
        .collect();

    let device_id_ref = device.device_id.clone();
    let cache_ref = cache.clone();

    let file_results: Vec<_> = futures::stream::iter(files)
        .map(|path| {
            let client = client.clone();
            let parent_uid = sync_folder_uid.clone();
            let remote_names = remote_names.clone();
            let did = device_id_ref.clone();
            let c = cache_ref.clone();
            async move {
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if let Some(ref c) = c {
                    if c.is_file_unchanged_since_sync(&did, &path) {
                        return ('s', name);
                    }
                } else if remote_names.contains(&name) {
                    return ('s', name);
                }
                let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let pb = upload_progress_bar(&name, file_size);
                let result = client.upload_file(&path, parent_uid, false, progress_callback(pb.clone())).await;
                pb.set_position(file_size);
                finish_progress(&pb);
                match result {
                    Ok(_) => {
                        if let Some(ref c) = c { let _ = c.mark_file_synced(&did, &path); }
                        ('u', name)
                    }
                    Err(e) => { eprintln!("  Upload failed for '{}': {}", name, e); ('e', name) }
                }
            }
        })
        .buffer_unordered(UPLOAD_CONCURRENCY)
        .collect()
        .await;

    for (status, _name) in file_results {
        match status { 'u' => uploaded += 1, 's' => skipped += 1, _ => errors += 1 }
    }

    // ── Recursive directory sync ──────────────────────────────────────────
    for entry in entries.iter().filter(|e| e.path().is_dir()) {
        let path = entry.path();
        let sub_name = path.file_name().unwrap().to_string_lossy().to_string();
        let remote_sub_uid = if let Some(n) = sync_folder_children.iter().find(|n| n.base().name == sub_name) {
            n.uid().clone()
        } else {
            let sp = new_spinner(format!("Creating remote folder '{}'...", sub_name));
            match client.create_folder(sync_folder_uid.clone(), sub_name.clone(), None).await {
                Ok(f) => { sp.finish_and_clear(); f.base.uid }
                Err(e) => { sp.finish_and_clear(); eprintln!("  Failed to create '{}': {e}", sub_name); errors += 1; continue; }
            }
        };
        let (u, s, e) = sync_folder_recursive(&client, &path, remote_sub_uid, &device.device_id, cache.clone()).await;
        uploaded += u; skipped += s; errors += e;
    }

    println!("\n  Sync complete — {} uploaded, {} skipped, {} error(s).", uploaded, skipped, errors);

    Ok(())
}

/// Returns (uploaded, skipped, errors). Uploads files in parallel.
fn sync_folder_recursive<'a>(
    client: &'a proton_drive_sdk::client::ProtonDriveClient,
    local_path: &'a std::path::Path,
    remote_uid: NodeUid,
    device_id: &'a str,
    cache: Option<Arc<crate::rusqlite_cache::RusqliteCache>>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = (usize, usize, usize)> + Send + 'a>> {
    Box::pin(async move {
        let existing: Vec<Node> = match list_children(client, remote_uid.clone()).await {
            Ok(v) => v,
            Err(_) => return (0, 0, 1),
        };
        let remote_names: std::collections::HashSet<String> =
            existing.iter().map(|n| n.base().name.clone()).collect();

        let entries: Vec<_> = match std::fs::read_dir(local_path) {
            Ok(it) => it.flatten().collect(),
            Err(_) => return (0, 0, 1),
        };

        let mut uploaded = 0usize;
        let mut skipped = 0usize;
        let mut errors = 0usize;

        let files: Vec<_> = entries.iter()
            .filter(|e| e.path().is_file())
            .map(|e| e.path())
            .collect();

        let file_results: Vec<_> = futures::stream::iter(files)
            .map(|path| {
                let client_ref = client.clone();
                let remote_uid = remote_uid.clone();
                let remote_names = remote_names.clone();
                let cache_ref = cache.clone();
                async move {
                    let name = path.file_name().unwrap().to_string_lossy().to_string();
                    // Local change-tracking is the primary skip signal.
                    if let Some(ref c) = cache_ref {
                        if c.is_file_unchanged_since_sync(device_id, &path) {
                            return ('s', name);
                        }
                    } else if remote_names.contains(&name) {
                        return ('s', name);
                    }
                    let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let pb = upload_progress_bar(&name, file_size);
                    let result = client_ref.upload_file(&path, remote_uid, false, progress_callback(pb.clone())).await;
                    pb.set_position(file_size);
                    finish_progress(&pb);
                    match result {
                        Ok(_) => {
                            if let Some(ref c) = cache_ref { let _ = c.mark_file_synced(device_id, &path); }
                            ('u', name)
                        }
                        Err(e) => { eprintln!("  Upload failed for '{}': {}", name, e); ('e', name) }
                    }
                }
            })
            .buffer_unordered(UPLOAD_CONCURRENCY)
            .collect()
            .await;

        for (status, _) in file_results {
            match status { 'u' => uploaded += 1, 's' => skipped += 1, _ => errors += 1 }
        }

        for entry in entries.iter().filter(|e| e.path().is_dir()) {
            let path = entry.path();
            let sub_name = path.file_name().unwrap().to_string_lossy().to_string();
            let sub_uid = if let Some(n) = existing.iter().find(|n| n.base().name == sub_name) {
                n.uid().clone()
            } else {
                match client.create_folder(remote_uid.clone(), sub_name, None).await {
                    Ok(f) => f.base.uid,
                    Err(_) => { errors += 1; continue; }
                }
            };
            let (u, s, e) = sync_folder_recursive(client, &path, sub_uid, device_id, cache.clone()).await;
            uploaded += u; skipped += s; errors += e;
        }

        (uploaded, skipped, errors)
    })
}

fn get_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output().ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "My Computer".to_string())
}
