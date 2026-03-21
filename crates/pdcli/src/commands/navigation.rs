use crate::state::ReplState;
use anyhow::{anyhow, Result};
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::photo::ProtonPhotosClient;
use std::sync::Arc;
use parking_lot::Mutex;
use proton_drive_sdk::client::ProtonDriveClient;

use super::helpers::{
    area_from_path, format_size,
    has_wildcards, list_children, new_spinner, resolve_folder_path, selector_matches, Area,
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
        let sp = new_spinner("Fetching trash...");
        let raw = client.enumerate_trash().await?;
        sp.finish_and_clear();
        nodes = raw.into_iter().filter_map(|r| r.ok()).collect();
        for n in &nodes { let _ = cache.upsert_node(n, true); }
    } else {
        // Resolve full Node objects from cache-backed data where possible,
        // falling back to cached display for items not yet decrypted.
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
        let top_entries = ["MyFiles", "Trash", "Photos", "Computers"];
        let mut visible = Vec::new();
        for entry in top_entries {
            if selector_matches(selector.unwrap_or("*"), entry)? {
                visible.push(entry);
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

    if area == Area::Computers {
        return list_computers_flat(&client, selector).await;
    }

    let current_uid = current_uid.ok_or_else(|| anyhow!("No current directory"))?;

    if let Some(sel) = selector {
        if sel == "Trash" || sel == "/Trash" {
            return list_trash_flat(&client, &root_uid, &cache, None).await;
        }
        if has_wildcards(sel) {
            let children = list_children(&client, current_uid.clone()).await?;
            let mut shown = 0usize;
            for node in &children {
                if !selector_matches(sel, &node.base().name)? { continue; }
                shown += 1;
                match node {
                    Node::Folder(_) | Node::Album(_) => println!("  {:>8}  {}/", "[DIR]", node.base().name),
                    Node::File(f) => {
                        let size = format_size(f.active_revision.size_on_cloud_storage.max(0) as u64);
                        println!("  {:>8}  {}  ({})", size, f.base.base.name, f.base.media_type);
                    }
                    Node::Photo(f) => {
                        let size = format_size(f.active_revision.size_on_cloud_storage.max(0) as u64);
                        println!("  {:>8}  {}  [photo]", size, f.base.base.name);
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
        let fetched = list_children(&client, target_uid.clone()).await?;
        for node in &fetched { cache.upsert_node(node, false)?; }
        display_nodes(&fetched);
    } else {
        display_cached_nodes(&cached_children);
    }
    println!("\n  {} item(s)\n", if cached_children.is_empty() { 0 } else { cached_children.len() });
    Ok(())
}

async fn list_photos_flat(
    state: &Arc<Mutex<ReplState>>,
    selector: Option<&str>,
) -> Result<()> {
    let photos = {
        let s = state.lock();
        let session = s
            .get_session()
            .ok_or_else(|| anyhow!("Not authenticated"))?;
        ProtonPhotosClient::new(session, None)?
    };

    let sp = new_spinner("Fetching photo timeline...");
    let items = photos.iterate_timeline().await?;
    sp.finish_and_clear();

    if items.is_empty() {
        println!("\n  Photos/\n\n  (empty)\n");
        return Ok(());
    }

    println!("\n  Photos/\n");
    let mut shown = 0usize;
    for item in &items {
        let name = item.uid.link_id.raw().to_string();
        if let Some(sel) = selector {
            if !selector_matches(sel, &name)? { continue; }
        }
        shown += 1;
        let ts = item.capture_time.format("%Y-%m-%d %H:%M").to_string();
        println!("  {:>8}  {}  [{}]", "[PHOTO]", name, ts);
    }
    println!("\n  {} item(s)\n", shown);
    Ok(())
}

async fn list_computers_flat(
    client: &ProtonDriveClient,
    selector: Option<&str>,
) -> Result<()> {
    let sp = new_spinner("Fetching computers...");
    let devices = client.list_devices().await?;
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

fn display_nodes(nodes: &[Node]) {    for node in nodes {
        match node {
            Node::Folder(_) | Node::Album(_) => println!("  {:>8}  {}/", "[DIR]", node.base().name),
            Node::File(f) | Node::Photo(f) => {
                let size = format_size(f.active_revision.size_on_cloud_storage.max(0) as u64);
                println!("  {:>8}  {}", size, node.base().name);
            }
        }
    }
}

fn display_cached_nodes(nodes: &[crate::rusqlite_cache::CachedNode]) {
    for node in nodes {
        let kind = if node.node_type == "Folder" || node.node_type == "Album" { "[DIR]" } else { "FILE" };
        let size = if kind == "[DIR]" { "".to_string() } else { format_size(node.size.unwrap_or(0).max(0) as u64) };
        println!("  {:>8}  {}{}", size, node.name, if kind == "[DIR]" { "/" } else { "" });
    }
}

pub async fn cd_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() { return Err(anyhow!("Usage: cd <path>")); }
    let target = args[0];

    let (_client, current_uid, root_uid, current_path, area, cache) = {
        let s = state.lock();
        let client = s.get_client().ok_or_else(|| anyhow!("Not authenticated"))?.clone();
        let uid = s.get_current_node_uid().cloned();
        let root = s.get_root_node_uid().cloned();
        let path = s.get_current_path().to_vec();
        let area = area_from_path(&path);
        let cache = s.get_cache().ok_or_else(|| anyhow!("Cache unknown"))?;
        (client, uid, root, path, area, cache)
    };

    let mut new_path = if target.starts_with('/') { Vec::new() } else { current_path };
    let mut new_uid = match (target.starts_with('/'), area) {
        (true, _) => None,
        (false, Area::MyFiles) => current_uid,
        _ => None,
    };

    for seg in target.split('/').filter(|s| !s.is_empty() && *s != ".") {
        match seg {
            ".." => {
                new_path.pop();
                if new_path.is_empty() { new_uid = None; }
                else if let Some(uid) = new_uid.clone() {
                    if let Ok(Some(c)) = cache.get_node_by_uid(&uid.volume_id, &uid.link_id) {
                        if let Some(p) = c.parent_link_id {
                            new_uid = Some(NodeUid::new(uid.volume_id.clone(), LinkId::new(p)));
                        }
                    }
                }
            }
            "~" => { new_path = vec!["MyFiles".to_string()]; new_uid = root_uid.clone(); }
            "MyFiles" => { new_path = vec!["MyFiles".to_string()]; new_uid = root_uid.clone(); }
            "Trash" => { new_path = vec!["Trash".to_string()]; new_uid = None; }
            "Photos" => { new_path = vec!["Photos".to_string()]; new_uid = None; }
            "Computers" => { new_path = vec!["Computers".to_string()]; new_uid = None; }
            name => {
                if let Some(uid) = new_uid.clone() {
                    if let Ok(Some(node)) = cache.get_child_by_name(&uid.volume_id, Some(&uid.link_id), name) {
                        new_uid = Some(NodeUid::new(uid.volume_id.clone(), LinkId::new(node.link_id)));
                        new_path.push(name.to_string());
                    } else { return Err(anyhow!("Folder not found: {}", name)); }
                } else { return Err(anyhow!("Navigation not supported from here")); }
            }
        }
    }

    let mut s = state.lock();
    if let Some(u) = new_uid { s.set_current_node_uid(u); }
    else { s.clear_current_node_uid(); }
    s.set_current_path(new_path);
    Ok(())
}
