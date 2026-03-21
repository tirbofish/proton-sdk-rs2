use anyhow::Result;
use std::sync::Arc;
use parking_lot::Mutex;
use crate::state::ReplState;
use crate::fuse::ProtonDriveFS;
use std::path::PathBuf;
use fuser::MountOption;
use super::helpers::new_spinner;
use super::sync::run_events_loop;
use proton_drive_sdk::photo::ProtonPhotosClient;
use proton_drive_sdk::links::LinkId;

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

    // Resolve Photos volume and root folder (best-effort; FUSE still mounts if this fails).
    let sp_meta = new_spinner("Resolving Photos & Computers metadata...");
    let (photos_volume_id, photos_root_link_id) = match photos_client {
        Some(photos) => match photos.get_photos_root_folder().await {
            Ok(root_folder) => {
                let vid = root_folder.base.uid.volume_id.clone();
                let lid = root_folder.base.uid.link_id.clone();
                (Some(vid), Some(lid))
            }
            Err(e) => {
                eprintln!("  [mount] Photos unavailable: {e}");
                (None, None)
            }
        },
        None => (None, None),
    };

    // Resolve Computers list (best-effort).
    let computers: Vec<(String, String, proton_drive_sdk::volume::VolumeId, LinkId)> =
        match client.list_devices().await {
            Ok(devices) => devices
                .into_iter()
                .map(|d| {
                    let root_link = d.root_uid.link_id.clone();
                    (d.device_id, d.name, d.volume_id, root_link)
                })
                .collect(),
            Err(e) => {
                eprintln!("  [mount] Computers unavailable: {e}");
                vec![]
            }
        };
    sp_meta.finish_and_clear();

    let fs = ProtonDriveFS {
        client: client.clone(),
        cache: cache.clone(),
        volume_id: volume_id.clone(),
        root_link_id: root_link_id.clone(),
        photos_volume_id,
        photos_root_link_id,
        computers,
        mount_point: mount_point.clone(),
        pending_downloads: Default::default(),
        pending_uploads: Default::default(),
        next_fh: 0,
        probe_fhs: Default::default(),
        probe_last_seen: Default::default(),
        intent_confirmed: Default::default(),
    };

    let options = vec![
        MountOption::FSName("proton-drive".to_string()),
    ];

    let sp = new_spinner(format!("Mounting Proton Drive to {}...", mount_point.display()));

    // spawn_mount2 runs FUSE in a background thread and returns immediately.
    // Dropping the session unmounts the filesystem.
    let session = fuser::spawn_mount2(fs, &mount_point, &options)?;
    let notifier = Arc::new(session.notifier());

    // Record the active mount point so the REPL can auto-unmount on exit.
    state.lock().set_mount_point(Some(mount_point.clone()));
    state.lock().set_sync_status(Some("FUSE Active".to_string()));

    // Start the events loop, wired to the notifier so server-side changes
    // immediately invalidate kernel dentry/inode caches (no navigate-away needed).
    let events_state = state.clone();
    let events_client = client.clone();
    let events_vid = volume_id.clone();
    let events_cache = cache.clone();
    let events_rli = root_link_id.clone();
    let events_notifier = notifier.clone();
    let events_task = tokio::spawn(async move {
        eprintln!("  [FUSE] Events loop started — polling every 5s");
        if let Err(e) = run_events_loop(
            events_client, events_vid, events_cache, None, events_state,
            Some(events_rli), Some(events_notifier),
        ).await {
            eprintln!("  [FUSE] Events loop ended: {}", e);
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    sp.finish_and_clear();
    println!("  Mounted at {}  (Ctrl+C to unmount)", mount_point.display());

    tokio::signal::ctrl_c().await?;

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
