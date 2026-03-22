use crate::state::ReplState;
use anyhow::{anyhow, Result};
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::volume::VolumeId;
use proton_drive_sdk::photo::ProtonPhotosClient;
use proton_drive_sdk::utils::PotentialObject;
use futures::StreamExt;use std::sync::Arc;
use parking_lot::Mutex;
use proton_drive_sdk::client::ProtonDriveClient;

use super::helpers::{
    area_from_path, format_size,
    has_wildcards, new_spinner, resolve_folder_path, selector_matches, Area,
};

pub async fn pwd_command(state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let s = state.lock();
    println!("{}", s.current_path_display());
    Ok(())
}

async fn list_trash_flat(
    client: &ProtonDriveClient,
    root_uid: &NodeUid,
    cache: &Arc<crate::rusqlite_cache::RusqliteCache>,
    selector: Option<&str>,
) -> Result<()> {
    let volume_id = root_uid.volume_id.clone();

    // Use cached items if we have them; otherwise fetch from server.
    let cached = cache.list_trash(&volume_id).unwrap_or_default();
    let nodes: Vec<Node>;

    if cached.is_empty() {
        let sp = new_spinner("Fetching trash (Ctrl+C to abort)...");
        let raw = tokio::select! {
            res = client.enumerate_trash() => res?,
            _ = tokio::signal::ctrl_c() => {
                sp.finish_and_clear();
                println!("  (interrupted)");
                return Ok(());
            }
        };
        sp.finish_and_clear();
        nodes = raw.into_iter().filter_map(|r| r.ok()).collect();
        for n in &nodes { let _ = cache.upsert_node(n, true); }
    } else {
        println!("\n  Trash/\n");
        let mut shown = 0usize;
        for item in &cached {
            let name = &item.name;
            if let Some(sel) = selector {
                if !selector_matches(sel, name)? { continue; }
            }
            shown += 1;
            let is_dir = item.node_type == "Folder" || item.node_type == "Album";
            if is_dir {
                println!("  {:>8}  {}/", "[DIR]", name);
            } else {
                let size = format_size(item.size.unwrap_or(0).max(0) as u64);
                println!("  {:>8}  {}", size, name);
            }
        }
        println!("\n  {} item(s)\n", shown);
        return Ok(());
    }

    if nodes.is_empty() {
        println!("\n  Trash is empty.\n");
        return Ok(());
    }

    println!("\n  Trash/\n");
    let mut shown = 0usize;
    for node in &nodes {
        let name = &node.base().name;
        if let Some(sel) = selector {
            if !selector_matches(sel, name)? { continue; }
        }
        shown += 1;
        match node {
            Node::Folder(_) | Node::Album(_) => println!("  {:>8}  {}/", "[DIR]", name),
            Node::File(f) | Node::Photo(f) => {
                let size = format_size(f.active_revision.size_on_cloud_storage.max(0) as u64);
                println!("  {:>8}  {}", size, name);
            }
        }
    }
    println!("\n  {} item(s)\n", shown);
    Ok(())
}

pub async fn ls_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.len() > 1 {
        return Err(anyhow!("Usage: ls [path|pattern]"));
    }
    let selector = args.first().copied();

    let (client, current_uid, root_uid, current_path, area, cache) = {
        let s = state.lock();
        let client = s.get_client().ok_or_else(|| anyhow!("Not authenticated"))?.clone();
        let uid = s.get_current_node_uid().cloned();
        let root_uid = s.get_root_node_uid().ok_or_else(|| anyhow!("Root unknown"))?.clone();
        let current_path = s.get_current_path().to_vec();
        let area = area_from_path(&current_path);
        let cache = s.get_cache().ok_or_else(|| anyhow!("Cache unknown"))?;
        (client, uid, root_uid, current_path, area, cache)
    };

    if area == Area::Top {
        // If the selector exactly names a virtual folder, list its contents
        // instead of just showing the directory entry.
        if let Some(sel) = selector {
            match sel {
                "Photos" | "Photos/" => return list_photos_flat(&state, None).await,
                "Trash" | "Trash/" => return list_trash_flat(&client, &root_uid, &cache, None).await,
                "Computers" | "Computers/" => return list_computers_flat(&client, None).await,
                "MyFiles" | "MyFiles/" => {
                    // Fall through to normal folder listing via current_uid
                }
                _ => {}
            }
        }

        let top_entries = ["MyFiles", "Trash", "Photos", "Computers"];
        let mut visible = Vec::new();
        for entry in top_entries {
            if selector_matches(selector.unwrap_or("*"), entry)? {
                visible.push(entry);
            }
        }
        // If exactly one entry matched and it was MyFiles, list its contents.
        if visible.len() == 1 && visible[0] == "MyFiles" && selector.is_some() {
            let s = state.lock();
            if let Some(root) = s.get_root_node_uid() {
                let cached = cache.list_children(&root.volume_id, Some(&root.link_id))?;
                drop(s);
                if !cached.is_empty() {
                    println!("\n  MyFiles/\n");
                    display_cached_nodes(&cached);
                    println!("\n  {} item(s)\n", cached.len());
                    return Ok(());
                }
            }
        }
        println!("\n  /\n");
        for entry in &visible {
            println!("  {:>8}  {}/", "[DIR]", entry);
        }
        println!("\n  {} item(s)\n", visible.len());
        return Ok(());
    }

    if area == Area::Trash {
        return list_trash_flat(&client, &root_uid, &cache, selector).await;
    }

    if area == Area::Photos {
        return list_photos_flat(&state, selector).await;
    }

    if area == Area::Computers && current_path.len() <= 1 {
        return list_computers_flat(&client, selector).await;
    }

    let current_uid = current_uid.ok_or_else(|| anyhow!("No current directory"))?;

    if let Some(sel) = selector {
        if sel == "Trash" || sel == "/Trash" {
            return list_trash_flat(&client, &root_uid, &cache, None).await;
        }
        if has_wildcards(sel) {
            let stream = client.enumerate_folder_children(current_uid.clone()).await?;
            tokio::pin!(stream);
            let mut shown = 0usize;
            let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
            loop {
                tokio::select! {
                    biased;
                    _ = &mut ctrl_c => {
                        println!("\n  (interrupted \u{2014} {} item(s) shown)\n", shown);
                        return Ok(());
                    }
                    item = stream.next() => match item {
                        None => break,
                        Some(Ok(PotentialObject::Node(node))) => {
                            let name = node.base().name.clone();
                            if !selector_matches(sel, &name)? { continue; }
                            shown += 1;
                            display_node(&node);
                        }
                        Some(Ok(PotentialObject::Degraded(_))) => {}
                        Some(Err(e)) => eprintln!("  Warning: {e}"),
                    }
                }
            }
            println!("\n  {} item(s)\n", shown);
            return Ok(());
        }
    }

    let (target_uid, target_path): (NodeUid, Vec<String>) = if let Some(path) = selector {
        resolve_folder_path(&client, current_uid, current_path, root_uid, path, Some(&*cache)).await?
    } else {
        (current_uid, current_path)
    };

    println!("\n  {}/\n", target_path.join("/"));
    let cached_children = cache.list_children(&target_uid.volume_id, Some(&target_uid.link_id))?;
    if cached_children.is_empty() {
        let stream = client.enumerate_folder_children(target_uid.clone()).await?;
        tokio::pin!(stream);
        let mut count = 0usize;
        let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
        loop {
            tokio::select! {
                biased;
                _ = &mut ctrl_c => {
                    println!("\n  (interrupted \u{2014} {} item(s))\n", count);
                    return Ok(());
                }
                item = stream.next() => match item {
                    None => break,
                    Some(Ok(PotentialObject::Node(node))) => {
                        let _ = cache.upsert_node(&node, false);
                        display_node(&node);
                        count += 1;
                    }
                    Some(Ok(PotentialObject::Degraded(_))) => { count += 1; }
                    Some(Err(e)) => eprintln!("  Warning: {e}"),
                }
            }
        }
        println!("\n  {} item(s)\n", count);
    } else {
        display_cached_nodes(&cached_children);
        println!("\n  {} item(s)\n", cached_children.len());
    }
    Ok(())
}

async fn list_photos_flat(
    state: &Arc<Mutex<ReplState>>,
    selector: Option<&str>,
) -> Result<()> {
    let (photos, cache, photos_vid) = {
        let s = state.lock();
        let session = s.get_session().ok_or_else(|| anyhow!("Not authenticated"))?;
        let cache = s.get_cache().ok_or_else(|| anyhow!("Cache not initialized"))?;
        let photos_vid = s.get_photos_root_node_uid().map(|u| u.volume_id.clone());
        let photos = ProtonPhotosClient::new(session, None)?;
        (photos, cache, photos_vid)
    };

    // Try cache first — photos are already sorted by capture_time DESC.
    if let Some(ref vid) = photos_vid {
        let albums = cache.list_albums(vid).unwrap_or_default();
        let all_photos = cache.list_all_photos(vid).unwrap_or_default();
        if !albums.is_empty() || !all_photos.is_empty() {
            // Build a display list: albums first, then photos.
            let mut items: Vec<(String, String, String)> = Vec::new(); // (size_col, date_col, name_col)
            for item in &albums {
                let name = &item.name;
                if let Some(sel) = selector {
                    if !selector_matches(sel, name)? { continue; }
                }
                items.push(("[DIR]".to_string(), String::new(), format!("{}/", name)));
            }
            for item in &all_photos {
                let name = &item.name;
                if let Some(sel) = selector {
                    if !selector_matches(sel, name)? { continue; }
                }
                let size = format_size(item.size.unwrap_or(0).max(0) as u64);
                let date = item.capture_time
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                items.push((size, date, name.clone()));
            }
            display_paged("Photos", &items);
            return Ok(());
        }
    }

    let sp = new_spinner("Connecting to Photos...");
    let bootstrap = tokio::select! {
        r = async {
            let root = photos.get_photos_root_folder().await?;
            let vid  = photos.get_photos_volume_id().await?;
            Ok::<_, anyhow::Error>((root, vid))
        } => r,
        _ = tokio::signal::ctrl_c() => {
            sp.finish_and_clear();
            println!("  (interrupted)");
            return Ok(());
        }
    };
    let (root, volume_id) = bootstrap?;
    let root_link_id = root.base.uid.link_id.clone();
    sp.finish_and_clear();

    let sp = new_spinner("Loading Photos…  discovering items");
    let stream = photos.enumerate_children(volume_id, Some(root_link_id)).await?;
    tokio::pin!(stream);

    let mut items: Vec<(String, u64, Option<chrono::DateTime<chrono::Utc>>, bool)> = Vec::new();
    let mut total = 0usize;
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            biased;
            _ = &mut ctrl_c => {
                sp.finish_and_clear();
                println!("\n  (interrupted — {} item(s) fetched)\n", total);
                return Ok(());
            }
            item = stream.next() => match item {
                None => break,
                Some(Ok(PotentialObject::Node(node))) => {
                    total += 1;
                    let name = node.base().name.clone();
                    sp.set_message(format!("Loading Photos…  {} items — {}", total, &name[..name.len().min(30)]));
                    let (size, is_dir) = match &node {
                        Node::File(f) | Node::Photo(f) => (f.active_revision.size_on_cloud_storage.max(0) as u64, false),
                        Node::Folder(_) | Node::Album(_) => (0, true),
                    };
                    // capture_time lives on photo metadata, not NodeBase
                    let ct: Option<chrono::DateTime<chrono::Utc>> = None;
                    items.push((name, size, ct, is_dir));
                }
                Some(Ok(PotentialObject::Degraded(_))) => {
                    total += 1;
                    sp.set_message(format!("Loading Photos…  {} items", total));
                    items.push(("[encrypted]".to_string(), 0, None, false));
                }
                Some(Err(e)) => eprintln!("  Warning: {e}"),
            }
        }
    }

    sp.finish_and_clear();

    // Sort by capture_time descending (newest first), None at the end.
    items.sort_by(|a, b| b.2.cmp(&a.2));

    let display_items: Vec<(String, String, String)> = items.iter().map(|(name, size, ct, is_dir)| {
        if *is_dir {
            ("[DIR]".to_string(), String::new(), format!("{}/", name))
        } else {
            let size_str = format_size(*size);
            let date = ct.map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
            (size_str, date, name.clone())
        }
    }).collect();

    display_paged("Photos", &display_items);
    Ok(())
}

async fn list_computers_flat(
    client: &ProtonDriveClient,
    selector: Option<&str>,
) -> Result<()> {
    let sp = new_spinner("Fetching computers (Ctrl+C to abort)...");
    let devices = tokio::select! {
        res = client.list_devices() => res?,
        _ = tokio::signal::ctrl_c() => {
            sp.finish_and_clear();
            println!("  (interrupted)");
            return Ok(());
        }
    };
    sp.finish_and_clear();

    println!("\n  Computers/\n");
    if devices.is_empty() {
        println!("  (no computers registered)\n");
        return Ok(());
    }

    let mut shown = 0usize;
    for device in &devices {
        let name = &device.name;
        if let Some(sel) = selector {
            if !selector_matches(sel, name)? { continue; }
        }
        shown += 1;
        println!("  {:>8}  {}/", "[DIR]", name);
    }
    println!("\n  {} item(s)\n", shown);
    Ok(())
}

fn display_node(node: &Node) {
    match node {
        Node::Folder(_) | Node::Album(_) => println!("  {:>8}  {}/", "[DIR]", node.base().name),
        Node::File(f) | Node::Photo(f) => {
            let size = format_size(f.active_revision.size_on_cloud_storage.max(0) as u64);
            println!("  {:>8}  {}", size, node.base().name);
        }
    }
}

/// Paginated display with arrow-key scrolling for large listings.
/// Each item is `(size_col, date_col, name_col)`.
fn display_paged(title: &str, items: &[(String, String, String)]) {
    if items.is_empty() {
        println!("\n  {}/\n\n  (empty)\n", title);
        return;
    }

    // Get terminal height; use 24 as fallback.
    let page_size = crossterm::terminal::size()
        .map(|(_, h)| (h as usize).saturating_sub(4).max(5))
        .unwrap_or(20);

    if items.len() <= page_size {
        // Everything fits on one screen — just print it.
        println!("\n  {}/\n", title);
        for (size, date, name) in items {
            println!("  {:>8}  {:20}  {}", size, date, name);
        }
        println!("\n  {} item(s)\n", items.len());
        return;
    }

    // Interactive paged mode.
    let total = items.len();
    let mut offset = 0usize;

    if crossterm::terminal::enable_raw_mode().is_err() {
        // Fallback to plain print if we can't enter raw mode.
        println!("\n  {}/\n", title);
        for (size, date, name) in items {
            println!("  {:>8}  {:20}  {}", size, date, name);
        }
        println!("\n  {} item(s)\n", total);
        return;
    }

    use crossterm::{cursor, execute, terminal};
    use std::io::Write;
    let mut stdout = std::io::stdout();

    loop {
        let end = (offset + page_size).min(total);
        // Clear screen and draw the page.
        let _ = execute!(stdout, terminal::Clear(terminal::ClearType::All), cursor::MoveTo(0, 0));
        println!("  {}/  ({}-{} of {})\r", title, offset + 1, end, total);
        println!("\r");
        for (size, date, name) in &items[offset..end] {
            println!("  {:>8}  {:20}  {}\r", size, date, name);
        }
        println!("\r");
        print!("  ↑/↓ scroll, q quit\r");
        let _ = stdout.flush();

        // Wait for a key event.
        loop {
            if let Ok(event) = crossterm::event::read() {
                match event {
                    crossterm::event::Event::Key(key) => match key.code {
                        crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Char('j') => {
                            if offset + page_size < total { offset += 1; }
                            break;
                        }
                        crossterm::event::KeyCode::Up | crossterm::event::KeyCode::Char('k') => {
                            if offset > 0 { offset -= 1; }
                            break;
                        }
                        crossterm::event::KeyCode::PageDown => {
                            offset = (offset + page_size).min(total.saturating_sub(page_size));
                            break;
                        }
                        crossterm::event::KeyCode::PageUp => {
                            offset = offset.saturating_sub(page_size);
                            break;
                        }
                        crossterm::event::KeyCode::Home => { offset = 0; break; }
                        crossterm::event::KeyCode::End => { offset = total.saturating_sub(page_size); break; }
                        crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc => {
                            let _ = crossterm::terminal::disable_raw_mode();
                            let _ = execute!(stdout, terminal::Clear(terminal::ClearType::All), cursor::MoveTo(0, 0));
                            println!("  {} item(s)", total);
                            return;
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    }
}

fn display_cached_nodes(nodes: &[crate::rusqlite_cache::CachedNode]) {
    for node in nodes {
        let is_dir = node.node_type == "Folder" || node.node_type == "Album";
        if is_dir {
            println!("  {:>8}  {}/", "[DIR]", node.name);
        } else {
            let size = format_size(node.size.unwrap_or(0).max(0) as u64);
            println!("  {:>8}  {}", size, node.name);
        }
    }
}

pub async fn cd_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() { return Err(anyhow!("Usage: cd <path>")); }
    let target = args[0];

    let (client, current_uid, root_uid, current_path, area, cache, computers) = {
        let s = state.lock();
        let client = s.get_client().ok_or_else(|| anyhow!("Not authenticated"))?.clone();
        let uid = s.get_current_node_uid().cloned();
        let root = s.get_root_node_uid().cloned();
        let path = s.get_current_path().to_vec();
        let area = area_from_path(&path);
        let cache = s.get_cache().ok_or_else(|| anyhow!("Cache unknown"))?;
        let computers = s.get_computers().to_vec();
        (client, uid, root, path, area, cache, computers)
    };

    let mut new_path = if target.starts_with('/') { Vec::new() } else { current_path.clone() };
    let mut computer_volume: Option<VolumeId> = if area == Area::Computers && current_path.len() > 1 {
        current_uid.as_ref().map(|u| u.volume_id.clone())
    } else {
        None
    };
    let mut new_uid = match (target.starts_with('/'), area) {
        (true, _) => None,
        (false, Area::MyFiles) => current_uid,
        (false, Area::Computers) if current_path.len() > 1 => current_uid,
        _ => None,
    };

    for seg in target.split('/').filter(|s| !s.is_empty() && *s != ".") {
        match seg {
            ".." => {
                new_path.pop();
                if new_path.is_empty() {
                    new_uid = None;
                    computer_volume = None;
                } else if new_path.len() == 1 && new_path[0] == "Computers" {
                    new_uid = None;
                    computer_volume = None;
                } else if let Some(uid) = new_uid.clone() {
                    let lookup_vol = computer_volume.as_ref().unwrap_or(&uid.volume_id);
                    if let Ok(Some(c)) = cache.get_node_by_uid(lookup_vol, &uid.link_id) {
                        if let Some(p) = c.parent_link_id {
                            new_uid = Some(NodeUid::new(lookup_vol.clone(), LinkId::new(p)));
                        }
                    }
                }
            }
            "~" => { new_path = vec!["MyFiles".to_string()]; new_uid = root_uid.clone(); }
            "MyFiles" => { new_path = vec!["MyFiles".to_string()]; new_uid = root_uid.clone(); }
            "Trash" => { new_path = vec!["Trash".to_string()]; new_uid = None; }
            "Photos" => { new_path = vec!["Photos".to_string()]; new_uid = None; }
            "Computers" => { new_path = vec!["Computers".to_string()]; new_uid = None; computer_volume = None; }
            name => {
                if new_path.len() == 1 && new_path[0] == "Computers" {
                    if let Some((_, _, vol, root)) = computers.iter().find(|(_, n, _, _)| n == name) {
                        computer_volume = Some(vol.clone());
                        new_uid = Some(NodeUid::new(vol.clone(), root.clone()));
                        new_path.push(name.to_string());
                        continue;
                    }
                    return Err(anyhow!("Computer '{}' not found. Run 'computers ls' to see available computers.", name));
                }
                if let Some(uid) = new_uid.clone() {
                    let lookup_vol = computer_volume.as_ref().unwrap_or(&uid.volume_id);
                    if let Ok(Some(node)) = cache.get_child_by_name(lookup_vol, Some(&uid.link_id), name) {
                        new_uid = Some(NodeUid::new(lookup_vol.clone(), LinkId::new(node.link_id)));
                        new_path.push(name.to_string());
                    } else { return Err(anyhow!("Folder not found: {}", name)); }
                } else { return Err(anyhow!("Navigation not supported from here")); }
            }
        }
    }

    // Spawn background indexing for the destination folder if its children aren't cached yet.
    if let Some(ref uid) = new_uid {
        let cached = cache.list_children(&uid.volume_id, Some(&uid.link_id)).unwrap_or_default();
        if cached.is_empty() {
            let bg_client = client.clone();
            let bg_uid = uid.clone();
            let bg_cache = cache.clone();
            tokio::spawn(async move {
                if let Ok(stream) = bg_client.enumerate_folder_children(bg_uid).await {
                    tokio::pin!(stream);
                    while let Some(item) = stream.next().await {
                        if let Ok(PotentialObject::Node(node)) = item {
                            let _ = bg_cache.upsert_node(&node, false);
                        }
                    }
                }
            });
        }
    }

    let mut s = state.lock();
    if let Some(u) = new_uid { s.set_current_node_uid(u); }
    else { s.clear_current_node_uid(); }
    s.set_current_path(new_path);
    Ok(())
}
