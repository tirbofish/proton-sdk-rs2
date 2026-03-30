use console::{pad_str, style, Alignment};
use proton_drive_sdk::node::{DegradedNode, Node};
use proton_drive_sdk::utils::PotentialObject;

use crate::app::AppState;
use crate::vfs::{VfsSection, VirtualPath};

pub async fn pwd(state: &AppState) -> anyhow::Result<()> {
    eprintln!("{}", state.cwd_display());
    Ok(())
}

pub async fn ls(args: &[String], state: &AppState) -> anyhow::Result<()> {
    // Strip --refresh / -r flag before any other processing.
    let refresh = args.iter().any(|a| a == "--refresh" || a == "-r");
    let args: Vec<&String> = args.iter().filter(|a| *a != "--refresh" && *a != "-r").collect();

    // Separate path navigation arg from glob pattern.
    // If the first arg contains no glob metacharacters, it's a path to list.
    // If it contains glob chars, it's a filter on the current directory.
    let raw_arg = args.first().map(|s| s.trim_end_matches('/'));
    let (effective_cwd, pattern): (std::borrow::Cow<VirtualPath>, Option<&str>) =
        match raw_arg {
            Some(a) if !a.is_empty() && !a.contains(['*', '?', '[']) => {
                let target = VirtualPath::resolve(&state.cwd, a);
                (std::borrow::Cow::Owned(target), None)
            }
            Some(a) if !a.is_empty() => {
                // glob pattern — list current dir filtered by pat
                (std::borrow::Cow::Borrowed(&state.cwd), Some(a))
            }
            _ => (std::borrow::Cow::Borrowed(&state.cwd), None),
        };
    let effective_cwd = effective_cwd.as_ref();

    match &effective_cwd.section {
        VfsSection::Root => {
            print_entry(true, "MyFiles", None);
            print_entry(true, "Trash", None);
            print_entry(true, "Computers", None);
            print_entry(true, "Photos", None);
            return Ok(());
        }
        VfsSection::Trash => {
            return list_trash(state).await;
        }
        VfsSection::Computers if effective_cwd.components.is_empty() => {
            return list_computers(pattern, refresh, state).await;
        }
        VfsSection::Computers => { /* /Computers/<device>/... — fall through to normal folder ls */ }
        VfsSection::Photos
            if effective_cwd.components.len() == 1
                && effective_cwd.components[0].eq_ignore_ascii_case("All") =>
        {
            return list_all_photos(state).await;
        }
        VfsSection::Photos if !effective_cwd.components.is_empty() => {
            return list_album_photos(effective_cwd, state).await;
        }
        _ => {}
    }

    let uid = state
        .resolve_uid(effective_cwd)
        .await?
        .ok_or_else(|| anyhow::anyhow!("ls: {}: no such directory", raw_arg.unwrap_or(".")))?;

    // For the Photos root: use album-only index, don't enumerate all photos.
    if effective_cwd.section == VfsSection::Photos && effective_cwd.components.is_empty() {
        if refresh { state.index.unmark_indexed(&uid); }
        state.ensure_photos_root_indexed().await?;
        let entries = state.index.get_children(&uid);
        // Synthetic "All" folder — the full photo timeline.
        print_entry(true, "All", None);
        for entry in &entries {
            // Only show album nodes (is_folder), not stale individual photo files.
            if entry.is_folder {
                print_entry(true, &entry.name, None);
            }
        }
        return Ok(());
    }

    if refresh {
        state.index.unmark_indexed(&uid);
    }
    state.ensure_children_loaded(&uid).await?;

    let entries = if let Some(pat) = pattern {
        state.index.match_glob(&uid, pat)
    } else {
        state.index.get_children(&uid)
    };

    if entries.is_empty() {
        eprintln!("{}", style("(empty)").dim());
    }
    for entry in &entries {
        print_entry(entry.is_folder, &entry.name, entry.size);
    }

    Ok(())
}

fn print_entry(is_folder: bool, name: &str, size: Option<i64>) {
    eprintln!("{}", format_entry_colored(is_folder, name, size, console::Color::White));
}

fn format_entry_colored(is_folder: bool, name: &str, size: Option<i64>, color: console::Color) -> String {
    let tag = if is_folder {
        style("[DIR] ").fg(color).blue().bold().to_string()
    } else {
        style("[file]").fg(color).dim().to_string()
    };
    let size_s = size.map(format_size).unwrap_or_default();
    let padded = pad_str(name, 48, Alignment::Left, Some("…"));
    format!("{}  {}  {}", tag, style(&padded).fg(color), style(&size_s).fg(color).dim())
}

pub async fn cd(args: &[String], state: &mut AppState) -> anyhow::Result<()> {
    let input = args.first().map(|s| s.as_str()).unwrap_or("~");
    let target = VirtualPath::resolve(&state.cwd, input);

    match &target.section {
        VfsSection::Root => {
            if target.components.is_empty() {
                state.cwd = target;
            } else {
                // Path was not recognised as a known section — e.g. "cd COmputers".
                eprintln!("cd: {}: operation not allowed", input);
            }
        }
        VfsSection::Trash | VfsSection::Computers if target.components.is_empty() => {
            state.cwd = target;
        }
        VfsSection::Computers => {
            // Navigating into /Computers/<device>/... — validate the device name.
            let device_name = &target.components[0];
            let cached = state.devices.read();
            let found = cached.iter().any(|d| d.name.eq_ignore_ascii_case(device_name));
            drop(cached);
            if found || {
                // Not cached — fetch once to check.
                match state.drive.list_devices().await {
                    Ok(devs) => {
                        let ok = devs.iter().any(|d| d.name.eq_ignore_ascii_case(device_name));
                        *state.devices.write() = devs;
                        ok
                    }
                    Err(_) => false,
                }
            } {
                state.cwd = target;
            } else {
                eprintln!("cd: {}: unknown computer", device_name);
            }
        }
        VfsSection::Trash => {
            if target.components.is_empty() {
                state.cwd = target;
            } else {
                eprintln!("cd: /Trash has no sub-directories to navigate into");
            }
        }
        VfsSection::MyFiles | VfsSection::Photos => {
            // "All" is a synthetic virtual directory showing all photos in the timeline.
            if target.section == VfsSection::Photos
                && target.components.len() == 1
                && target.components[0].eq_ignore_ascii_case("All")
            {
                state.cwd = VirtualPath {
                    section: VfsSection::Photos,
                    components: vec!["All".to_string()],
                };
                return Ok(());
            }
            match state.resolve_uid(&target).await? {
                Some(_) => state.cwd = target,
                None => eprintln!("cd: {}: no such directory", input),
            }
        }
    }
    Ok(())
}

async fn list_trash(state: &AppState) -> anyhow::Result<()> {
    use futures::StreamExt;
    use crate::app::TrashRecord;

    let pb = crate::ui::spinner("Loading trash…");
    let stream = state.drive.stream_trash();
    tokio::pin!(stream);
    let mut count = 0usize;
    let mut records: Vec<TrashRecord> = Vec::new();

    while let Some(result) = stream.next().await {
        match result? {
            Ok(node) => {
                let (name, is_dir, uid) = match &node {
                    Node::Folder(f) | Node::Album(f) => (f.base.name.clone(), true, node.uid().clone()),
                    Node::File(f) | Node::Photo(f) => (f.base.base.name.clone(), false, node.uid().clone()),
                };
                pb.println(format_entry_colored(is_dir, &name, None, console::Color::Red));
                records.push(TrashRecord { uid, name, is_folder: is_dir });
            }
            Err(degraded) => {
                let (name, is_dir, uid) = match &degraded {
                    DegradedNode::Folder(f) | DegradedNode::Album(f) => {
                        let n = match &f.base.name {
                            PotentialObject::Node(s) => s.clone(),
                            PotentialObject::Degraded(_) => "<encrypted>".to_string(),
                        };
                        (n, true, degraded.uid().clone())
                    }
                    DegradedNode::File(f) | DegradedNode::Photo(f) => {
                        let n = match &f.base.name {
                            PotentialObject::Node(s) => s.clone(),
                            PotentialObject::Degraded(_) => "<encrypted>".to_string(),
                        };
                        (n, false, degraded.uid().clone())
                    }
                };
                let display = format!("{} {}", name, style("(degraded)").red());
                pb.println(format_entry_colored(is_dir, &display, None, console::Color::Red));
                records.push(TrashRecord { uid, name, is_folder: is_dir });
            }
        }
        count += 1;
    }

    *state.trash_items.write() = records.clone();
    // Persist to SQLite so the next session starts with a warm cache.
    let cache_rows: Vec<_> = records.iter().map(|r| (r.uid.clone(), r.name.clone(), r.is_folder)).collect();
    state.index.save_trash_cache(&cache_rows);
    pb.finish_and_clear();
    if count == 0 {
        eprintln!("{}", style("(empty)").dim());
    }
    Ok(())
}

async fn list_computers(filter: Option<&str>, refresh: bool, state: &AppState) -> anyhow::Result<()> {
    // Use the cached device list unless it's empty (first call) or --refresh is passed.
    let cached = state.devices.read().clone();
    let devices = if !cached.is_empty() && !refresh {
        cached
    } else {
        let pb = crate::ui::spinner("Fetching registered computers…");
        let devs = match state.drive.list_devices().await {
            Ok(v) => { pb.finish_and_clear(); v }
            Err(e) => { pb.finish_and_clear(); return Err(e); }
        };
        // Persist to SQLite for next session.
        let rows: Vec<_> = devs.iter().map(|d| crate::db::DeviceCacheRow {
            device_id: d.device_id.clone(),
            name: d.name.clone(),
            root_uid: d.root_uid.clone(),
            device_type_raw: d.device_type as u32,
            last_sync_time_rfc: d.last_sync_time.map(|t| t.to_rfc3339()),
        }).collect();
        state.index.save_devices_cache(&rows);
        *state.devices.write() = devs.clone();
        devs
    };
    if devices.is_empty() {
        eprintln!("{}", style("(no computers registered)").dim());
        return Ok(());
    }

    // If a device name was provided, enumerate that device's root folder.
    if let Some(name) = filter {
        let device = devices.iter().find(|d| d.name.eq_ignore_ascii_case(name));
        match device {
            None => eprintln!("ls: no computer named '{name}'"),
            Some(d) => {
                state.ensure_children_loaded(&d.root_uid).await?;
                let entries = state.index.get_children(&d.root_uid);
                if entries.is_empty() {
                    eprintln!("{}", style("(nothing synced yet)").dim());
                } else {
                    for entry in &entries {
                        print_entry(entry.is_folder, &entry.name, entry.size);
                    }
                }
            }
        }
        return Ok(());
    }

    // No filter — list all devices.
    for d in &devices {
        let last = d
            .last_sync_time
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "never".to_string());
        let meta = format!("{:?}  last sync {}", d.device_type, last);
        let padded = pad_str(&d.name, 40, Alignment::Left, Some("…"));
        eprintln!("{}  {}  {}", style("[DEV] ").cyan().bold(), padded, style(&meta).dim());
    }
    Ok(())
}

fn format_size(bytes: i64) -> String {
    let b = bytes as f64;
    if b < 1024.0 {
        format!("{b:.0} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.1} KB", b / 1024.0)
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Lists all photos in the account timeline (the virtual "All" album).
async fn list_all_photos(state: &AppState) -> anyhow::Result<()> {
    crate::photos_pager::show_photos_pager(state).await
}

async fn list_album_photos(cwd: &VirtualPath, state: &AppState) -> anyhow::Result<()> {
    state.ensure_photos_root_indexed().await?;
    let album_name = cwd.components.first().map(|s| s.as_str()).unwrap_or("Album");
    let uid = state
        .resolve_uid(cwd)
        .await?
        .ok_or_else(|| anyhow::anyhow!("ls: {}: album not found", album_name))?;
    state.ensure_album_children_loaded(&uid).await?;
    crate::photos_pager::show_album_pager(state, uid).await
}
