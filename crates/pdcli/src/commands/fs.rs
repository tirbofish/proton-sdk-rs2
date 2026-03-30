use std::path::PathBuf;

use crate::app::AppState;
use crate::index::IndexEntry;
use crate::vfs::VirtualPath;

/// Returns true if an error is an API "already exists" (error code 2500).
fn is_already_exists(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("2500") || s.to_lowercase().contains("already exists")
}

pub async fn mkdir(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let name = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("mkdir: missing folder name"))?
        .clone();

    let cwd = state.cwd.clone();
    let parent_uid = state
        .resolve_uid(&cwd)
        .await?
        .ok_or_else(|| anyhow::anyhow!("mkdir: cannot resolve current directory"))?;

    let pb = crate::ui::spinner(format!("Creating \"{}\"…", name));
    let folder = match state.drive.create_folder(parent_uid.clone(), name.clone(), None).await {
        Ok(f) => { pb.finish_and_clear(); f }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    };
    state.index.insert(IndexEntry {
        uid: folder.base.uid,
        parent_uid: Some(parent_uid.clone()),
        name: name.clone(),
        is_folder: true,
        size: None,
        modification_time: None,
        media_type: None,
    });
    // Invalidate parent's cached children so `cd <name>` / `ls` re-fetches from server.
    state.index.unmark_indexed(&parent_uid);
    crate::ui::ok(format!("Created '{name}'"));
    Ok(())
}

pub async fn mv(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let (src_arg, dst_arg) = parse_two_paths(args, "mv")?;
    let src = VirtualPath::resolve(&state.cwd, src_arg);
    let dst = VirtualPath::resolve(&state.cwd, dst_arg);

    let src_uid = state
        .resolve_uid(&src)
        .await?
        .ok_or_else(|| anyhow::anyhow!("mv: source not found: {}", src))?;

    match state.resolve_uid(&dst).await? {
        Some(dst_uid) => {
            let old_parent = state.index.get(&src_uid).and_then(|e| e.parent_uid.clone());
            state.index.reparent(&src_uid, &dst_uid);
            let pb = crate::ui::spinner("Moving…");
            let result = state.drive.move_nodes(vec![src_uid.clone()], dst_uid).await;
            pb.finish_and_clear();
            if let Err(e) = result {
                if let Some(orig) = old_parent {
                    state.index.reparent(&src_uid, &orig);
                }
                return Err(e);
            }
        }
        None => {
            let dst_name = dst
                .components
                .last()
                .ok_or_else(|| anyhow::anyhow!("mv: invalid destination path: {}", dst))?
                .clone();
            let dst_parent = VirtualPath {
                section: dst.section.clone(),
                components: dst.components[..dst.components.len() - 1].to_vec(),
            };
            let dst_parent_uid = state
                .resolve_uid(&dst_parent)
                .await?
                .ok_or_else(|| anyhow::anyhow!("mv: destination directory not found: {}", dst_parent))?;

            let old_entry = state.index.get(&src_uid);
            let old_parent = old_entry.as_ref().and_then(|e| e.parent_uid.clone());
            let old_name = old_entry.as_ref().map(|e| e.name.clone());
            let needs_move = old_parent.as_ref() != Some(&dst_parent_uid);
            let needs_rename = old_name.as_deref() != Some(&dst_name);

            if needs_move {
                state.index.reparent(&src_uid, &dst_parent_uid);
            }
            if needs_rename {
                state.index.rename_entry(&src_uid, dst_name.clone());
            }

            let pb = crate::ui::spinner("Moving…");
            let move_result = if needs_move {
                state.drive.move_nodes(vec![src_uid.clone()], dst_parent_uid).await
            } else {
                Ok(())
            };
            let rename_result = if move_result.is_ok() && needs_rename {
                state.drive.rename_node(src_uid.clone(), dst_name, None).await
            } else {
                Ok(())
            };
            pb.finish_and_clear();

            if let Err(e) = move_result.and(rename_result) {
                if let Some(orig) = old_parent {
                    state.index.reparent(&src_uid, &orig);
                }
                if let Some(orig_name) = old_name {
                    state.index.rename_entry(&src_uid, orig_name);
                }
                return Err(e);
            }
        }
    }
    crate::ui::ok("Moved");
    Ok(())
}

pub async fn cp(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let (src_arg, dst_arg) = parse_two_paths(args, "cp")?;
    let src = VirtualPath::resolve(&state.cwd, src_arg);
    let dst = VirtualPath::resolve(&state.cwd, dst_arg);

    let src_uid = state
        .resolve_uid(&src)
        .await?
        .ok_or_else(|| anyhow::anyhow!("cp: source not found: {}", src))?;

    let (dst_parent_uid, new_name) = match state.resolve_uid(&dst).await? {
        Some(uid) => (uid, None),
        None => {
            let dst_name = dst
                .components
                .last()
                .ok_or_else(|| anyhow::anyhow!("cp: invalid destination path: {}", dst))?
                .clone();
            let dst_parent = VirtualPath {
                section: dst.section.clone(),
                components: dst.components[..dst.components.len() - 1].to_vec(),
            };
            let parent_uid = state
                .resolve_uid(&dst_parent)
                .await?
                .ok_or_else(|| anyhow::anyhow!("cp: destination directory not found: {}", dst_parent))?;
            (parent_uid, Some(dst_name))
        }
    };

    let pb = crate::ui::spinner("Copying…");
    let new_link_id = match state.drive.copy_node(src_uid, dst_parent_uid.clone(), new_name).await {
        Ok(id) => { pb.finish_and_clear(); id }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    };
    let new_uid = proton_drive_sdk::node::NodeUid::new(state.volume_id.clone(), new_link_id);
    state.index.unmark_indexed(&dst_parent_uid);

    state.ensure_children_loaded(&dst_parent_uid).await?;
    let _ = new_uid;
    Ok(())
}

pub async fn rm(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let pattern = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| anyhow::anyhow!("rm: missing pattern or name"))?
        .as_str();

    let cwd = state.cwd.clone();
    let parent_uid = state
        .resolve_uid(&cwd)
        .await?
        .ok_or_else(|| anyhow::anyhow!("rm: cannot resolve current directory"))?;

    state.ensure_children_loaded(&parent_uid).await?;
    let targets = state.index.match_glob(&parent_uid, pattern);
    if targets.is_empty() {
        eprintln!("rm: no match: {}", pattern);
        return Ok(());
    }

    let snap = state.index.snapshot_children(&parent_uid);
    let uids: Vec<_> = targets.iter().map(|e| e.uid.clone()).collect();
    for uid in &uids {
        state.index.remove(uid);
    }

    let pb = crate::ui::spinner(format!("Moving {} item(s) to trash…", uids.len()));
    let results = match state.drive.trash_nodes(uids.clone()).await {
        Ok(r) => { pb.finish_and_clear(); r }
        Err(e) => { pb.finish_and_clear(); state.index.restore_snapshot(snap); return Err(e); }
    };
    let mut failed = false;
    for (uid, res) in &results {
        if let Err(e) = res {
            eprintln!("rm: failed to trash {uid}: {e}");
            failed = true;
        }
    }
    if failed {
        state.index.restore_snapshot(snap);
    } else {
        crate::ui::ok(format!("Trashed {} item(s)", uids.len()));
    }
    Ok(())
}

pub async fn get(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let remote_arg = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("get: missing remote path"))?
        .as_str();
    let local_arg = args.get(1).map(|s| s.as_str());

    let remote = VirtualPath::resolve(&state.cwd, remote_arg);
    let uid = state
        .resolve_uid(&remote)
        .await?
        .ok_or_else(|| anyhow::anyhow!("get: not found: {}", remote))?;

    let entry = state
        .index
        .get(&uid)
        .ok_or_else(|| anyhow::anyhow!("get: node not in index"))?;

    // ── Directory download ────────────────────────────────────────────────────
    if entry.is_folder {
        return get_directory(uid, &entry.name, local_arg, state).await;
    }

    let local_path = resolve_local_path(local_arg, &entry.name);

    let total = entry.size.unwrap_or(0);
    let pb = crate::ui::download_bar(total);
    let pb_cb = pb.clone();

    state
        .drive
        .download_to_file(
            uid,
            &local_path,
            Box::new(move |done, _total| {
                pb_cb.set_position(done as u64);
            }),
        )
        .await?;

    pb.finish_and_clear();
    crate::ui::ok(format!("Downloaded → {}", local_path.display()));
    Ok(())
}

async fn get_directory(
    folder_uid: proton_drive_sdk::node::NodeUid,
    folder_name: &str,
    local_arg: Option<&str>,
    state: &AppState,
) -> anyhow::Result<()> {
    let local_dir = match local_arg {
        Some(p) => expand_tilde(p),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(folder_name)
        }
    };
    std::fs::create_dir_all(&local_dir)?;

    state.ensure_children_loaded(&folder_uid).await?;
    let children = state.index.get_children(&folder_uid);
    let files: Vec<_> = children.iter().filter(|e| !e.is_folder).collect();

    if files.is_empty() {
        crate::ui::ok(format!("Folder '{}' is empty — created {}", folder_name, local_dir.display()));
        return Ok(());
    }

    eprintln!("Downloading {} file(s) into {}…", files.len(), local_dir.display());
    let mut ok_count = 0usize;

    for entry in files {
        let dest = local_dir.join(&entry.name);
        let total = entry.size.unwrap_or(0);
        let pb = crate::ui::download_bar(total);
        pb.set_message(entry.name.clone());
        let pb_cb = pb.clone();
        let uid = entry.uid.clone();

        match state
            .drive
            .download_to_file(
                uid,
                &dest,
                Box::new(move |done, _total| { pb_cb.set_position(done as u64); }),
            )
            .await
        {
            Ok(()) => {
                pb.finish_and_clear();
                crate::ui::ok(format!("↓ {}", entry.name));
                ok_count += 1;
            }
            Err(e) if is_already_exists(&e) => {
                pb.finish_and_clear();
                crate::ui::skip(format!("{} — already exists, skipping", entry.name));
            }
            Err(e) => {
                pb.finish_and_clear();
                eprintln!("  {} failed: {e}", entry.name);
            }
        }
    }

    crate::ui::ok(format!("Downloaded {ok_count} file(s) → {}", local_dir.display()));
    Ok(())
}

pub async fn put(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let local_arg = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("put: missing local file path"))?;

    let local_path = PathBuf::from(local_arg);
    if !local_path.exists() {
        anyhow::bail!("put: not found: {}", local_path.display());
    }

    // ── Directory upload ──────────────────────────────────────────────────────
    if local_path.is_dir() {
        return put_directory(&local_path, state).await;
    }

    let cwd = state.cwd.clone();
    let parent_uid = state
        .resolve_uid(&cwd)
        .await?
        .ok_or_else(|| anyhow::anyhow!("put: cannot resolve current directory"))?;

    let size = std::fs::metadata(&local_path)?.len() as i64;
    let pb = crate::ui::upload_bar(size);
    let file_display = local_path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    pb.set_message(file_display.clone());
    let pb_cb = pb.clone();

    let upload_result = state
        .drive
        .upload_file(
            &local_path,
            parent_uid.clone(),
            false,
            Box::new(move |done, _total| {
                pb_cb.set_position(done as u64);
            }),
        )
        .await;

    pb.finish_and_clear();

    let file_name = local_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match upload_result {
        Ok(node_uid) => {
            // Optimistically insert with local metadata — no get_node round-trip needed.
            state.index.insert(IndexEntry {
                uid: node_uid,
                parent_uid: Some(parent_uid),
                name: file_name.to_string(),
                is_folder: false,
                size: Some(size),
                modification_time: None,
                media_type: None,
            });
            crate::ui::ok(format!("Uploaded '{file_name}'"));
        }
        Err(e) if is_already_exists(&e) => {
            crate::ui::skip(format!("{file_name} — already exists, skipping"));
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

async fn put_directory(local_path: &PathBuf, state: &AppState) -> anyhow::Result<()> {
    let dir_name = local_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("put: cannot determine directory name"))?
        .to_string();

    let cwd = state.cwd.clone();
    let parent_uid = state
        .resolve_uid(&cwd)
        .await?
        .ok_or_else(|| anyhow::anyhow!("put: cannot resolve current directory"))?;

    // Create the remote folder, or reuse if it already exists.
    let pb = crate::ui::spinner(format!("Creating remote folder '{dir_name}'…"));
    let folder_uid = match state.drive.create_folder(parent_uid.clone(), dir_name.clone(), None).await {
        Ok(f) => {
            pb.finish_and_clear();
            let uid = f.base.uid.clone();
            state.index.insert(IndexEntry {
                uid: uid.clone(),
                parent_uid: Some(parent_uid.clone()),
                name: dir_name.clone(),
                is_folder: true,
                size: None,
                modification_time: None,
                media_type: None,
            });
            // Invalidate parent's cached children so `cd <dir_name>` works immediately.
            state.index.unmark_indexed(&parent_uid);
            crate::ui::ok(format!("Created remote folder '{dir_name}'"));
            uid
        }
        Err(e) if e.to_string().contains("2500") || e.to_string().to_lowercase().contains("already exists") => {
            pb.finish_and_clear();
            // Folder already exists on the server — fetch cwd children to get its UID.
            state.index.unmark_indexed(&parent_uid);
            state.ensure_children_loaded(&parent_uid).await?;
            let uid = state
                .index
                .find_child_by_name(&parent_uid, &dir_name)
                .ok_or_else(|| anyhow::anyhow!("put: folder '{dir_name}' exists on server but could not be resolved"))?;
            // Pre-load the folder's existing children so they're not lost after we mark_indexed.
            state.ensure_children_loaded(&uid).await?;
            uid
        }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    };

    // Collect direct files (non-recursive).
    let mut files: Vec<PathBuf> = std::fs::read_dir(local_path)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();

    if files.is_empty() {
        crate::ui::ok("No files to upload (directory is empty or contains only subdirectories)");
        return Ok(());
    }

    eprintln!("Uploading {} file(s)…", files.len());
    let mut uploaded = 0usize;
    let mut skipped = 0usize;

    for file in &files {
        let size = std::fs::metadata(file)?.len() as i64;
        let file_name = file.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let pb = crate::ui::upload_bar(size);
        pb.set_message(file_name.clone());
        let pb_cb = pb.clone();

        match state
            .drive
            .upload_file(
                file,
                folder_uid.clone(),
                false,
                Box::new(move |done, _total| { pb_cb.set_position(done as u64); }),
            )
            .await
        {
            Ok(node_uid) => {
                pb.finish_and_clear();
                // Optimistically insert with local metadata — no get_node round-trip.
                state.index.insert(IndexEntry {
                    uid: node_uid,
                    parent_uid: Some(folder_uid.clone()),
                    name: file_name.clone(),
                    is_folder: false,
                    size: Some(size),
                    modification_time: None,
                    media_type: None,
                });
                crate::ui::ok(format!("↑ {file_name}"));
                uploaded += 1;
            }
            Err(e) if is_already_exists(&e) => {
                pb.finish_and_clear();
                crate::ui::skip(format!("{file_name} — already exists, skipping"));
                skipped += 1;
            }
            Err(e) => {
                pb.finish_and_clear();
                eprintln!("  {file_name} failed: {e}");
            }
        }
    }

    let total = files.len();
    let summary = if skipped > 0 {
        format!("Uploaded {uploaded}/{total} file(s) into '{dir_name}' ({skipped} skipped)")
    } else {
        format!("Uploaded {uploaded}/{total} file(s) into '{dir_name}'")
    };
    crate::ui::ok(summary);
    // Mark the folder as indexed with the files we just inserted so
    // an immediate `ls` uses the in-memory cache without a server round-trip.
    // Skipped files were pre-loaded via ensure_children_loaded earlier,
    // so they are already in the in-memory children map.
    state.index.mark_indexed(&folder_uid);
    Ok(())
}

pub async fn touch(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let name = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("touch: missing file name"))?
        .clone();

    let cwd = state.cwd.clone();
    let parent_uid = state
        .resolve_uid(&cwd)
        .await?
        .ok_or_else(|| anyhow::anyhow!("touch: cannot resolve current directory"))?;

    let tmp = std::env::temp_dir().join(&name);
    std::fs::write(&tmp, b"")?;

    let pb = crate::ui::spinner(format!("Creating \"{}\"…", name));
    let result = state
        .drive
        .upload_file(
            &tmp,
            parent_uid.clone(),
            false,
            Box::new(|_, _| {}),
        )
        .await;
    pb.finish_and_clear();
    let _ = std::fs::remove_file(&tmp);
    let node_uid = result?;

    // Insert the new node directly — no full folder re-fetch.
    if let Ok(node) = state.drive.get_node(node_uid).await {
        state.index.insert_node(&node, Some(parent_uid));
    }
    crate::ui::ok(format!("Created '{name}'"));
    Ok(())
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(rest)
    } else if p == "~" {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
    } else {
        PathBuf::from(p)
    }
}

fn resolve_local_path(local_arg: Option<&str>, default_name: &str) -> PathBuf {
    match local_arg {
        Some(p) => expand_tilde(p),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(default_name)
        }
    }
}

fn parse_two_paths<'a>(args: &'a [String], cmd: &str) -> anyhow::Result<(&'a str, &'a str)> {
    match args {
        [src, dst, ..] => Ok((src.as_str(), dst.as_str())),
        _ => anyhow::bail!("{cmd}: requires two path arguments"),
    }
}
