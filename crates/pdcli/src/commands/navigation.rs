use crate::state::ReplState;
use anyhow::{anyhow, Result};
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::links::LinkId;
use std::sync::Arc;
use parking_lot::Mutex;
use std::time::Duration;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::utils::PotentialObject;

use super::helpers::{
    area_from_path, format_size,
    has_wildcards, list_children, resolve_folder_path, selector_matches, Area,
};

async fn enter_trash_loop(
    client: &ProtonDriveClient,
    root_uid: &NodeUid,
    cache: &Arc<crate::rusqlite_cache::RusqliteCache>,
) -> Result<()> {
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyEvent},
        execute,
        style::{Color, Stylize},
        terminal::{self, ClearType},
    };
    use proton_drive_sdk::node::VolumeTrashBatchLoader;
    use proton_drive_sdk::share_ops::ShareOperations;
    use std::io::stdout;

    let volume_id = root_uid.volume_id.clone();

    let page_size = 50;
    let mut selected_idx: usize = 0;

    // Pre-seed from the local SQLite cache so names appear immediately.
    let cached_trash = cache.list_trash(&volume_id).unwrap_or_default();
    let initial: Vec<(LinkId, Option<Node>, Option<String>)> = cached_trash
        .into_iter()
        .map(|n| {
            let lid = proton_drive_sdk::links::LinkId::new(n.link_id.clone());
            let cached_name = Some(n.name.clone());
            (lid, None, cached_name)
        })
        .collect();

    let items = Arc::new(parking_lot::RwLock::new(initial));

    let items_clone = items.clone();
    let client_clone = client.clone();
    let volume_id_clone = volume_id.clone();
    
    let decrypt_handle = tokio::spawn(async move {
        let mut page = 0;
        
        loop {
            let response = match client_clone.api().trash().get_trash(volume_id_clone.clone(), page_size, page).await {
                Ok(r) => r,
                Err(_) => break,
            };

            if response.trash.is_empty() { break; }

            for share_trash in response.trash {
                let share_and_key = match ShareOperations::get_share(&client_clone, share_trash.share_id).await {
                    Ok(sk) => sk,
                    Err(_) => continue,
                };

                let mut batch_loader = VolumeTrashBatchLoader::new(
                    Arc::new(client_clone.clone()),
                    volume_id_clone.clone(),
                    share_and_key.key,
                );

                {
                    // Add any link IDs not yet in the list (from server but not in cache).
                    let mut lock = items_clone.write();
                    for id in &share_trash.link_ids {
                        if !lock.iter().any(|(lid, _, _)| lid.raw() == id.raw()) {
                            lock.push((id.clone(), None, None));
                        }
                    }
                }

                for id in share_trash.link_ids {
                    if let Ok(res) = batch_loader.queue_and_try_load_batch(id).await {
                        for node_obj in res {
                            if let PotentialObject::Node(n) = node_obj {
                                let mut lock = items_clone.write();
                                if let Some(item) = lock.iter_mut().find(|(lid, _, _)| lid.raw() == n.uid().link_id.raw()) {
                                    item.1 = Some(n);
                                }
                            }
                        }
                    }
                }
                
                if let Ok(remaining) = batch_loader.load_remaining().await {
                    for node_obj in remaining {
                        if let PotentialObject::Node(n) = node_obj {
                            let mut lock = items_clone.write();
                            if let Some(item) = lock.iter_mut().find(|(lid, _, _)| lid.raw() == n.uid().link_id.raw()) {
                                item.1 = Some(n);
                            }
                        }
                    }
                }
            }
            page += 1;
        }
    });

    terminal::enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let res = loop {
        let current_items = items.read().clone();
        if current_items.is_empty() {
            execute!(stdout, terminal::Clear(ClearType::All), cursor::MoveTo(0, 0))?;
            println!(" Loading Trash...");
            tokio::time::sleep(Duration::from_millis(100)).await;
            if decrypt_handle.is_finished() && items.read().is_empty() {
                println!(" Trash is empty.");
                tokio::time::sleep(Duration::from_secs(1)).await;
                break Ok(());
            }
            continue;
        }

        let (term_width, term_height) = terminal::size()?;
        let visible_height = (term_height as usize).saturating_sub(6);
        let start_render = selected_idx.saturating_sub(visible_height / 2);

        execute!(stdout, terminal::Clear(ClearType::All), cursor::MoveTo(0, 0))?;
        println!("{}", " ProtonDrive Trash Browser ".bold().on_blue());
        println!(" (↑/↓: Navigate | d: Delete Permanently | q: Exit)");
        println!(" Items: {} | Terminal: {}x{}", current_items.len(), term_width, term_height);
        println!("{}", "─".repeat(term_width as usize).dark_grey());

        for i in 0..visible_height {
            let idx = start_render + i;
            if idx >= current_items.len() { break; }

            let (link_id, node_opt, cached_name) = &current_items[idx];
            let is_selected = idx == selected_idx;
            let prefix = if is_selected { " > " } else { "   " };

            let (display_name, size_str, is_loading) = match node_opt {
                Some(n) => (
                    n.base().name.clone(),
                    format_size(match n {
                        Node::File(f) | Node::Photo(f) => f.active_revision.size_on_cloud_storage.max(0) as u64,
                        _ => 0,
                    }),
                    false,
                ),
                None => {
                    // Use cached SQLite name if available, otherwise abbreviated link ID
                    let name = cached_name.clone().unwrap_or_else(|| {
                        let raw = link_id.raw();
                        format!("{}…", &raw[..raw.len().min(20)])
                    });
                    (name, "…".to_string(), true)
                }
            };

            let name_width = (term_width as usize).saturating_sub(25);
            let truncated_name = if display_name.len() > name_width {
                format!("{}...", &display_name[..name_width.saturating_sub(3)])
            } else {
                format!("{:<width$}", display_name, width=name_width)
            };

            let loading_tag = if is_loading { " *" } else { "  " };
            let line = format!("{}{} {} {:>12}", prefix, loading_tag, truncated_name, size_str);
            if is_selected {
                println!("{}", line.with(Color::Cyan).bold());
            } else if is_loading {
                println!("{}", line.dark_grey());
            } else {
                println!("{}", line);
            }
        }

        execute!(stdout, cursor::MoveTo(0, term_height - 1))?;
        if let Some((_, Some(n), _)) = current_items.get(selected_idx) {
            let size = match n {
                Node::File(f) | Node::Photo(f) => f.active_revision.size_on_cloud_storage,
                _ => 0,
            };
            print!(" Name: {} | Size: {}", n.base().name, format_size(size.max(0) as u64));
        } else if let Some((_, None, Some(name))) = current_items.get(selected_idx) {
            print!(" Name: {} (loading…)", name);
        } else {
            print!(" ID: {}", current_items[selected_idx].0.raw());
        }

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(KeyEvent { code, .. }) = event::read()? {
                match code {
                    KeyCode::Up => selected_idx = selected_idx.saturating_sub(1),
                    KeyCode::Down => if selected_idx + 1 < current_items.len() { selected_idx += 1; },
                    KeyCode::Char('d') => {
                        let lid = current_items[selected_idx].0.clone();
                        let uid = NodeUid::new(volume_id.clone(), lid.clone());
                        client.delete_nodes_from_trash(vec![uid]).await?;
                        items.write().retain(|(id, _, _)| id.raw() != lid.raw());
                        if selected_idx >= items.read().len() && !items.read().is_empty() {
                            selected_idx = items.read().len() - 1;
                        }
                    }
                    KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => break Ok(()),
                    _ => {}
                }
            }
        }
    };

    decrypt_handle.abort();
    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;
    terminal::disable_raw_mode()?;
    res
}

pub async fn pwd_command(state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let s = state.lock();
    println!("{}", s.current_path_display());
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
        let top_entries = ["MyFiles", "Trash", "Photos"];
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

    if area == Area::Trash && selector.is_none() {
        return enter_trash_loop(&client, &root_uid, &cache).await;
    }

    let current_uid = current_uid.ok_or_else(|| anyhow!("No current directory"))?;

    if let Some(sel) = selector {
        if sel == "Trash" || sel == "/Trash" {
            return enter_trash_loop(&client, &root_uid, &cache).await;
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

fn display_nodes(nodes: &[Node]) {
    for node in nodes {
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
            "MyFiles" => { new_path = vec!["MyFiles".to_string()]; new_uid = root_uid.clone(); }
            "Trash" => { new_path = vec!["Trash".to_string()]; new_uid = None; }
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
