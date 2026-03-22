use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use parking_lot::Mutex;
use crate::state::ReplState;
use crate::fuse::ProtonDriveFS;
use std::path::PathBuf;
use fuser::MountOption;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use super::helpers::{new_spinner, finish_ok};
use super::sync::{run_events_loop, run_computers_index, run_computers_index_continuous, run_minimal_sync, seed_and_index_myfiles, run_computer_folder_sync_continuous};
use crate::photos_index::run_photos_index_continuous;
use proton_drive_sdk::photo::ProtonPhotosClient;
use proton_drive_sdk::links::LinkId;

fn spin_style() -> ProgressStyle {
    ProgressStyle::with_template("  {spinner:.cyan.bold}  {msg}")
        .unwrap()
        .tick_strings(&["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"])
}

fn bg_spin_style() -> ProgressStyle {
    ProgressStyle::with_template("      {spinner:.yellow}  {msg:.dim}")
        .unwrap()
        .tick_strings(&["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"])
}

fn step(mp: &MultiProgress, msg: &str) -> ProgressBar {
    let pb = mp.add(ProgressBar::new_spinner());
    pb.set_style(spin_style());
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(200));
    pb
}

/// Create a standalone background spinner (not in a MultiProgress).
/// Uses a low redraw rate to avoid visual noise.
fn bg_spinner(msg: &str) -> Arc<ProgressBar> {
    let pb = ProgressBar::with_draw_target(
        None,
        indicatif::ProgressDrawTarget::stderr_with_hz(4),
    );
    pb.set_style(bg_spin_style());
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(200));
    Arc::new(pb)
}

pub async fn mount_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!("Usage: mount <mount_point>");
    }

    let mount_point = PathBuf::from(args[0]);
    if !mount_point.exists() {
        std::fs::create_dir_all(&mount_point)?;
    }

    if let Err(_) = std::fs::read_dir(&mount_point) {
        anyhow::bail!("Permission denied: cannot access mount point '{}'. Try a path in your home directory like '~/ProtonDrive'.", mount_point.display());
    }

    let (client, volume_id, root_link_id, cache, photos_client) = {
        let s = state.lock();
        let client = s.get_client().cloned().ok_or_else(|| anyhow::anyhow!("Not authenticated"))?;
        let root_uid = s.get_root_node_uid().cloned().ok_or_else(|| anyhow::anyhow!("Root node not found"))?;
        let cache = s.get_cache().ok_or_else(|| anyhow::anyhow!("Cache not initialized"))?;
        let photos_client = s
            .get_session()
            .and_then(|sess| ProtonPhotosClient::new(sess, None).ok());
        (client, root_uid.volume_id, root_uid.link_id, cache, photos_client)
    };

    // ── Shared progress display for the entire mount session ────────────────
    let mp = MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::stderr_with_hz(4));

    // ── Phase 1: Parallel metadata resolution ──────────────────────────────
    let pb_photos_meta = step(&mp, "[1/2] Resolving Photos library...");
    let pb_devs_meta   = step(&mp, "[2/2] Listing registered computers...");

    let (photos_result, devices) = tokio::join!(
        async {
            match photos_client {
                Some(photos) => match photos.get_photos_root_folder().await {
                    Ok(root_folder) => {
                        let vid = root_folder.base.uid.volume_id.clone();
                        let lid = root_folder.base.uid.link_id.clone();
                        (Some(photos), Some(vid), Some(lid))
                    }
                    Err(_) => (None, None, None),
                },
                None => (None, None, None),
            }
        },
        async {
            client.list_devices().await.unwrap_or_default()
        }
    );
    let (photos_for_index, photos_volume_id, photos_root_link_id) = photos_result;

    finish_ok(&pb_photos_meta, match &photos_for_index {
        Some(_) => "[1/2] Photos library ready",
        None    => "[1/2] Photos not available",
    });
    finish_ok(&pb_devs_meta, &format!("[2/2] {} computer(s) found", devices.len()));
    // Clear phase 1 bars so they don't re-render when background bars tick.
    mp.clear().ok();

    let computers: Vec<(String, String, proton_drive_sdk::volume::VolumeId, LinkId)> = devices
        .iter()
        .map(|d| (d.device_id.clone(), d.name.clone(), d.volume_id.clone(), d.root_uid.link_id.clone()))
        .collect();
    let comp_count = computers.len();

    // ── Phase 2: Mount FUSE ────────────────────────────────────────────────
    let pb_mount = new_spinner("Mounting Proton Drive...");

    let fs = ProtonDriveFS {
        client: client.clone(),
        cache: cache.clone(),
        volume_id: volume_id.clone(),
        root_link_id: root_link_id.clone(),
        photos_volume_id: photos_volume_id.clone(),
        photos_root_link_id: photos_root_link_id.clone(),
        computers,
        mount_point: mount_point.clone(),
        pending_downloads: Default::default(),
        pending_uploads: Default::default(),
        next_fh: 0,
        provisional_map: Default::default(),
    };

    let options = vec![MountOption::FSName("proton-drive".to_string())];
    let session = fuser::spawn_mount2(fs, &mount_point, &options)?;
    let notifier = Arc::new(session.notifier());

    state.lock().set_mount_point(Some(mount_point.clone()));
    state.lock().set_sync_status(Some("FUSE Active".to_string()));
    if let (Some(vid), Some(lid)) = (&photos_volume_id, &photos_root_link_id) {
        state.lock().set_photos_root_node_uid(proton_drive_sdk::node::NodeUid::new(vid.clone(), lid.clone()));
    }

    finish_ok(&pb_mount, &format!("Mounted — files accessible at {}", mount_point.display()));

    // ── Phase 3: Snapshot event cursor & background indexing ──────────────
    // Each background task gets its own standalone spinner to avoid
    // MultiProgress redraw conflicts.
    const BGI_COMPUTERS: u64 = 5;
    const BGI_COMP_BASE: u64 = 50;

    run_minimal_sync(&client, &volume_id, &cache, None).await.ok();

    let pb_bg_comp    = bg_spinner("Indexing Computers...");
    let pb_bg_myfiles = if !state.lock().myfiles_indexed() {
        Some(bg_spinner("Indexing MyFiles..."))
    } else {
        eprintln!("      ✓  MyFiles already indexed");
        None
    };
    let pb_bg_photos  = photos_for_index.is_some()
        .then(|| bg_spinner("Syncing Photos library..."));

    eprintln!("      (Ctrl+C to unmount)");

    let photos_task = if let Some(photos) = photos_for_index {
        if let (Some(vid), Some(lid)) = (&photos_volume_id, &photos_root_link_id) {
            let vid = vid.clone();
            let lid = lid.clone();
            let bg_cache    = cache.clone();
            let bg_notifier = notifier.clone();
            let pb = pb_bg_photos.as_ref().map(Arc::clone);
            Some(tokio::spawn(async move {
                run_photos_index_continuous(photos, bg_cache, vid, lid, Some(bg_notifier), pb).await;
            }))
        } else { None }
    } else { None };

    let bg_comp_task = {
        let bg_client   = client.clone();
        let bg_cache    = cache.clone();
        let bg_notifier = notifier.clone();
        let bg_devices  = devices;
        let pb = Arc::clone(&pb_bg_comp);
        tokio::spawn(async move {
            if let Err(e) = run_computers_index(&bg_client, &bg_devices, &bg_cache, Some(&pb)).await {
                eprintln!("      ✗  Computers index error: {e}");
            } else {
                let _ = bg_notifier.inval_inode(BGI_COMPUTERS, 0, -1);
                for i in 0..comp_count as u64 {
                    let _ = bg_notifier.inval_inode(BGI_COMP_BASE + i, 0, -1);
                }
                eprintln!("      ✓  Computers indexed");
            }
            pb.finish_and_clear();
            run_computers_index_continuous(
                bg_client, bg_devices, bg_cache,
                Some(bg_notifier), comp_count,
            ).await;
        })
    };

    let bg_myfiles_task = if let Some(pb) = pb_bg_myfiles {
        let bg_client   = client.clone();
        let bg_volume   = volume_id.clone();
        let bg_cache    = cache.clone();
        let bg_state    = state.clone();
        let bg_root     = root_link_id.clone();
        let bg_notifier = notifier.clone();
        Some(tokio::spawn(async move {
            seed_and_index_myfiles(
                &bg_client, &bg_volume, &bg_cache, &bg_state,
                Some(&bg_root), Some(&pb),
            ).await;
            let _ = bg_notifier.inval_inode(2u64, 0, -1);
            eprintln!("      ✓  MyFiles indexed");
            pb.finish_and_clear();
            bg_state.lock().set_myfiles_indexed(true);
        }))
    } else { None };

    // ── Events loop ───────────────────────────────────────────────────────
    let events_state    = state.clone();
    let events_client   = client.clone();
    let events_vid      = volume_id.clone();
    let events_cache    = cache.clone();
    let events_rli      = root_link_id.clone();
    let events_notifier = notifier.clone();
    let events_task = tokio::spawn(async move {
        if let Err(e) = run_events_loop(
            events_client, events_vid, events_cache, None, events_state,
            Some(events_rli), Some(events_notifier),
        ).await {
            tracing::warn!("Events loop ended: {}", e);
        }
    });

    // ── Resume persisted computer folder syncs ────────────────────────────
    let mut folder_sync_tasks = Vec::new();
    if let Ok(sync_configs) = cache.list_computer_sync_configs() {
        for (device_id, local_path) in sync_configs {
            if local_path.exists() {
                let bg_client = client.clone();
                let dev_id = device_id.clone();
                tracing::info!(device_id = %dev_id, path = %local_path.display(), "Resuming computer folder sync");
                folder_sync_tasks.push(tokio::spawn(async move {
                    run_computer_folder_sync_continuous(bg_client, dev_id, local_path).await;
                }));
            } else {
                tracing::warn!(device_id = %device_id, path = %local_path.display(), "Persisted sync path no longer exists, skipping");
            }
        }
    }

    tokio::signal::ctrl_c().await?;
    // Stop all background progress bars immediately so they don't keep printing.
    pb_bg_comp.finish_and_clear();
    if let Some(ref pb) = pb_bg_photos { pb.finish_and_clear(); }

    let sp = new_spinner(format!("Unmounting {}...", mount_point.display()));
    // -z = lazy unmount; succeeds even when device is busy.
    let output = std::process::Command::new("fusermount3")
        .arg("-u").arg("-z").arg(&mount_point)
        .output();
    // Drop the session regardless — this joins the FUSE background thread.
    drop(session);
    match output {
        Ok(o) if o.status.success() => {
            sp.finish_and_clear();
            println!("  Unmounted.");
        }
        Ok(o) => {
            sp.finish_with_message(format!(
                "Unmount busy (lazy mode active). stderr: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => {
            sp.finish_with_message(format!("fusermount3 error: {}", e));
        }
    }

    events_task.abort();
    bg_comp_task.abort();
    if let Some(t) = bg_myfiles_task { t.abort(); }
    if let Some(t) = photos_task { t.abort(); }
    for t in folder_sync_tasks {
        t.abort();
    }
    state.lock().set_sync_status(Some("FUSE Disconnected".to_string()));
    state.lock().set_mount_point(None);

    Ok(())
}

pub async fn umount_command(args: &[&str], _state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!("Usage: umount <mount_point>");
    }
    let path = args[0];
    let sp = new_spinner(format!("Unmounting {}...", path));
    let output = std::process::Command::new("fusermount3")
        .arg("-u")
        .arg("-z")
        .arg(path)
        .output()?;
    sp.finish_and_clear();
    if output.status.success() {
        println!("  Unmounted.");
    } else {
        anyhow::bail!("Unmount failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}
