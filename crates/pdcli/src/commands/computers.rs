use dialoguer::{theme::ColorfulTheme, Confirm};
use proton_drive_sdk::api::devices::DeviceType;
use proton_drive_sdk::node::NodeUid;
use walkdir::WalkDir;

use crate::app::AppState;
use crate::index::IndexEntry;

/// `sync [<local-path>] [--location <device-name>]`
///
/// Without a local-path: lists registered devices (original behaviour).
/// With a local-path: ensures a device entry exists for this machine (or
/// the provided `--location` name), creates a mirror folder on the device,
/// and uploads every file under the local tree.
pub async fn sync(args: &[String], state: &AppState) -> anyhow::Result<()> {
    // Parse flags.
    let location_idx = args.iter().position(|a| a == "--location" || a == "-l");
    let location_name = location_idx
        .and_then(|i| args.get(i + 1))
        .map(|s| s.clone());

    // Collect positional args (skip --location and its value).
    let skip_indices: std::collections::HashSet<usize> = location_idx
        .map(|i| [i, i + 1].into_iter().collect())
        .unwrap_or_default();
    let positional: Vec<&String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| !skip_indices.contains(i) && !a.starts_with('-'))
        .map(|(_, a)| a)
        .collect();

    let local_arg = positional.first().copied();

    // No local path → list devices (legacy behaviour).
    if local_arg.is_none() {
        let pb = crate::ui::spinner("Loading devices…");
        let devices = match state.drive.list_devices().await {
            Ok(v) => { pb.finish_and_clear(); v }
            Err(e) => { pb.finish_and_clear(); return Err(e); }
        };
        if devices.is_empty() {
            println!("No computers registered");
            return Ok(());
        }
        for d in &devices {
            let last = d
                .last_sync_time
                .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "never".to_string());
            println!(
                "{:<40} id={:<20} type={:?} last={}",
                d.name, d.device_id, d.device_type, last
            );
        }
        return Ok(());
    }

    let local_arg = local_arg.unwrap();

    // Expand `~`.
    let local_path = {
        let p = if let Some(rest) = local_arg.strip_prefix("~/") {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            std::path::PathBuf::from(home).join(rest)
        } else if local_arg == "~" {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
        } else {
            std::path::PathBuf::from(local_arg)
        };
        p
    };

    if !local_path.exists() {
        anyhow::bail!("sync: local path not found: {}", local_path.display());
    }

    // Determine device name: --location flag, or hostname, or panic.
    let device_name = match location_name {
        Some(n) => n,
        None => std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "my-computer".to_string()),
    };

    // Resolve the folder name from the local path.
    let folder_name = local_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("sync: cannot determine folder name from path"))?
        .to_string();

    // ── Step 1: find or create the device. ──────────────────────────────────
    let pb = crate::ui::spinner(format!("Looking up device \"{}\"…", device_name));
    let mut devices = state.drive.list_devices().await.map_err(|e| { pb.finish_and_clear(); e })?;
    *state.devices.write() = devices.clone();

    let device = match devices.iter().find(|d| d.name.eq_ignore_ascii_case(&device_name)) {
        Some(d) => {
            pb.set_message(format!("Found device \"{}\"", d.name));
            pb.finish_and_clear();
            d.clone()
        }
        None => {
            pb.set_message(format!("Registering new device \"{}\"…", device_name));
            let d = state.drive.create_device(device_name.clone(), DeviceType::Linux).await
                .map_err(|e| { pb.finish_and_clear(); e })?;
            pb.finish_and_clear();
            eprintln!("Registered new device '{}'", d.name);
            devices.push(d.clone());
            *state.devices.write() = devices.clone();
            // Persist updated device list to SQLite.
            let rows: Vec<_> = devices.iter().map(|dev| crate::db::DeviceCacheRow {
                device_id: dev.device_id.clone(),
                name: dev.name.clone(),
                root_uid: dev.root_uid.clone(),
                device_type_raw: dev.device_type as u32,
                last_sync_time_rfc: dev.last_sync_time.map(|t| t.to_rfc3339()),
            }).collect();
            state.index.save_devices_cache(&rows);
            d
        }
    };

    // ── Step 2: find or create the sync folder under the device root. ───────
    let pb = crate::ui::spinner(format!("Locating remote folder \"{}\"…", folder_name));
    state.ensure_children_loaded(&device.root_uid).await.map_err(|e| { pb.finish_and_clear(); e })?;

    let sync_folder_uid = match state.index.find_child_by_name(&device.root_uid, &folder_name) {
        Some(uid) => {
            pb.finish_and_clear();
            eprintln!("Using existing remote folder '{}'", folder_name);
            uid
        }
        None => {
            pb.set_message(format!("Creating remote folder \"{}\"…", folder_name));
            let folder = state.drive.create_folder(device.root_uid.clone(), folder_name.clone(), None).await
                .map_err(|e| { pb.finish_and_clear(); e })?;
            state.index.insert(IndexEntry {
                uid: folder.base.uid.clone(),
                parent_uid: Some(device.root_uid.clone()),
                name: folder_name.clone(),
                is_folder: true,
                size: None,
                modification_time: None,
                media_type: None,
            });
            pb.finish_and_clear();
            eprintln!("Created remote folder '{}'", folder_name);
            folder.base.uid
        }
    };

    // ── Step 3: walk and upload. ─────────────────────────────────────────────
    // Build a map from local sub-path to remote NodeUid so sub-directories are
    // created on-demand as we encounter them.
    let mut dir_uid_map: std::collections::HashMap<std::path::PathBuf, NodeUid> =
        std::collections::HashMap::new();
    dir_uid_map.insert(local_path.clone(), sync_folder_uid.clone());

    let entries: Vec<_> = WalkDir::new(&local_path)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .collect();

    let total_files = entries.iter().filter(|e| e.file_type().is_file()).count();
    eprintln!("Uploading {} file(s)…", total_files);

    let mut uploaded = 0usize;
    let mut skipped = 0usize;

    for entry in &entries {
        let entry_path = entry.path();
        let parent_local = entry_path.parent().unwrap_or(&local_path).to_path_buf();

        // Ensure the remote parent folder exists.
        let parent_uid = match dir_uid_map.get(&parent_local) {
            Some(uid) => uid.clone(),
            None => {
                // Create the directory chain from the deepest known ancestor.
                ensure_remote_dir(state, &local_path, &parent_local, &sync_folder_uid, &mut dir_uid_map).await?
            }
        };

        if entry.file_type().is_dir() {
            // Ensure this dir is registered in the map (will be created lazily by files beneath it).
            let dir_name = entry_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let _ = get_or_create_remote_dir(state, &parent_uid, dir_name.to_string()).await?;
            dir_uid_map.insert(entry_path.to_path_buf(), get_or_create_remote_dir(state, &parent_uid, dir_name.to_string()).await?);
            continue;
        }

        if !entry.file_type().is_file() {
            continue;
        }

        let file_name = entry_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Check if file already exists in remote (skip if so).
        state.ensure_children_loaded(&parent_uid).await?;
        if state.index.find_child_by_name(&parent_uid, file_name).is_some() {
            skipped += 1;
            continue;
        }

        let file_name_display = entry_path.strip_prefix(&local_path)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| file_name.to_string());

        let size = entry.metadata().map(|m| m.len() as i64).unwrap_or(0);
        let pb = crate::ui::upload_bar(size);
        pb.set_message(format!("{}", file_name_display));
        let pb_cb = pb.clone();

        let result = state.drive.upload_file(
            entry_path,
            parent_uid.clone(),
            false,
            Box::new(move |done, _total| { pb_cb.set_position(done as u64); }),
        ).await;

        match result {
            Ok(node_uid) => {
                pb.finish_and_clear();
                if let Ok(node) = state.drive.get_node(node_uid).await {
                    state.index.insert_node(&node, Some(parent_uid));
                }
                crate::ui::ok(format!("Uploaded {}", file_name_display));
                uploaded += 1;
            }
            Err(e) => {
                pb.finish_and_clear();
                let msg = e.to_string();
                if msg.contains("2500") || msg.to_lowercase().contains("already exists") {
                    eprintln!("  {} '{}': already exists, skipping",
                        console::style("skipped").yellow().dim(), file_name_display);
                    skipped += 1;
                } else {
                    eprintln!("  {} failed to upload '{}': {e}",
                        console::style("⚠").yellow(), file_name_display);
                }
            }
        }
    }

    crate::ui::ok(format!("Sync complete: {} uploaded, {} already existed", uploaded, skipped));
    Ok(())
}

/// Recursively ensures all directories from `local_root` to `local_dir` exist
/// remotely, adding each to `dir_uid_map`. Returns the remote UID for `local_dir`.
async fn ensure_remote_dir(
    state: &AppState,
    local_root: &std::path::Path,
    local_dir: &std::path::Path,
    remote_root: &NodeUid,
    dir_uid_map: &mut std::collections::HashMap<std::path::PathBuf, NodeUid>,
) -> anyhow::Result<NodeUid> {
    let rel = local_dir.strip_prefix(local_root).unwrap_or(local_dir);
    let components: Vec<_> = rel.components()
        .filter_map(|c| c.as_os_str().to_str().map(|s| s.to_string()))
        .collect();

    let mut current = remote_root.clone();
    let mut prefix = local_root.to_path_buf();

    for comp in &components {
        prefix = prefix.join(comp);
        if let Some(uid) = dir_uid_map.get(&prefix) {
            current = uid.clone();
        } else {
            let uid = get_or_create_remote_dir(state, &current, comp.clone()).await?;
            dir_uid_map.insert(prefix.clone(), uid.clone());
            current = uid;
        }
    }
    Ok(current)
}

async fn get_or_create_remote_dir(
    state: &AppState,
    parent_uid: &NodeUid,
    name: String,
) -> anyhow::Result<NodeUid> {
    state.ensure_children_loaded(parent_uid).await?;
    if let Some(uid) = state.index.find_child_by_name(parent_uid, &name) {
        return Ok(uid);
    }
    let folder = state.drive.create_folder(parent_uid.clone(), name.clone(), None).await?;
    state.index.insert(IndexEntry {
        uid: folder.base.uid.clone(),
        parent_uid: Some(parent_uid.clone()),
        name,
        is_folder: true,
        size: None,
        modification_time: None,
        media_type: None,
    });
    Ok(folder.base.uid)
}

/// `add <name>` registers the current machine as a Linux backup device.
pub async fn add(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let name = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("add: missing device name"))?
        .clone();

    let pb = crate::ui::spinner(format!("Registering \"{}\"…", name));
    let device = match state.drive.create_device(name.clone(), DeviceType::Linux).await {
        Ok(d) => { pb.finish_and_clear(); d }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    };
    crate::ui::ok(format!("Registered '{}' (id={})", device.name, device.device_id));
    Ok(())
}

/// `rename <old-name> <new-name>` renames a registered device.
pub async fn rename(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let (old_name, new_name) = match args {
        [o, n, ..] => (o.as_str(), n.as_str()),
        _ => anyhow::bail!("rename: requires <old-name> <new-name>"),
    };

    let pb = crate::ui::spinner("Loading devices…");
    let devices = match state.drive.list_devices().await {
        Ok(v) => { pb.finish_and_clear(); v }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    };
    let device = devices
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(old_name))
        .ok_or_else(|| anyhow::anyhow!("rename: no device named \'{}\'", old_name))?;

    let pb = crate::ui::spinner(format!("Renaming to \"{}\"…", new_name));
    let updated = match state.drive.rename_device(&device.device_id, new_name.to_string()).await {
        Ok(d) => { pb.finish_and_clear(); d }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    };
    crate::ui::ok(format!("Renamed to '{}'", updated.name));
    Ok(())
}

/// `rm [-f] <name>` removes a registered device (prompts for confirmation without -f).
pub async fn remove(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let force = args.iter().any(|a| a == "--force" || a == "-f");
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| anyhow::anyhow!("rm: missing device name"))?
        .as_str();

    let pb = crate::ui::spinner("Loading devices…");
    let devices = match state.drive.list_devices().await {
        Ok(v) => { pb.finish_and_clear(); v }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    };
    let device = devices
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow::anyhow!("rm: no device named \'{}\'", name))?;

    if !force {
        let confirmed = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!("Remove computer \'{}\' (id={})?", device.name, device.device_id))
            .default(false)
            .interact()?;
        if !confirmed {
            println!("Aborted");
            return Ok(());
        }
    }

    let pb = crate::ui::spinner(format!("Removing \'{}\'…", device.name));
    let device_id = device.device_id.clone();
    let device_name = device.name.clone();
    match state.drive.delete_device(&device_id).await {
        Ok(()) => { pb.finish_and_clear(); }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    }
    crate::ui::ok(format!("Removed '{}'", device_name));
    Ok(())
}
