use crate::state::ReplState;
use anyhow::{anyhow, Result};
use proton_drive_sdk::node::Node;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::helpers::{
    area_from_path, degraded_name, format_size,
    has_wildcards, list_children, resolve_folder_path, selector_matches, Area,
};

/// `pwd` — print current working directory
pub async fn pwd_command(state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let s = state.lock().await;
    println!("{}", s.current_path_display());
    Ok(())
}

/// `ls [path]` — list files and folders in the current directory
pub async fn ls_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.len() > 1 {
        return Err(anyhow!("Usage: ls [path|pattern]"));
    }
    let selector = args.first().copied();

    let (client, current_uid, root_uid, current_path, area) = {
        let s = state.lock().await;
        let client = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let uid = s.get_current_node_uid().cloned();
        let root_uid = s
            .get_root_node_uid()
            .ok_or_else(|| anyhow!("Root unknown; please re-login"))?
            .clone();
        let current_path = s.get_current_path().to_vec();
        let area = area_from_path(&current_path);
        (client, uid, root_uid, current_path, area)
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
        if visible.is_empty() {
            println!("  (empty)");
        } else {
            for entry in &visible {
                println!("  {:>8}  {}/", "[DIR]", entry);
            }
        }
        println!("\n  {} item(s)\n", visible.len());
        return Ok(());
    }

    if area == Area::Trash {
        println!("\n  Trash/\n");
        let items = client.enumerate_trash().await?;
        let mut shown = 0usize;
        if items.is_empty() {
            println!("  (empty)");
        } else {
            for item in &items {
                match item {
                    Ok(node) => match node {
                        Node::Folder(_) | Node::Album(_) => {
                            if !selector_matches(selector.unwrap_or("*"), &node.base().name)? {
                                continue;
                            }
                            shown += 1;
                            println!("  {:>8}  {}/", "[DIR]", node.base().name)
                        }
                        Node::File(f) => {
                            if !selector_matches(selector.unwrap_or("*"), &node.base().name)? {
                                continue;
                            }
                            shown += 1;
                            let size = format_size(f.active_revision.size_on_cloud_storage.max(0) as u64);
                            println!("  {:>8}  {}  ({})", size, node.base().name, f.base.media_type)
                        }
                        Node::Photo(f) => {
                            if !selector_matches(selector.unwrap_or("*"), &node.base().name)? {
                                continue;
                            }
                            shown += 1;
                            let size = format_size(f.active_revision.size_on_cloud_storage.max(0) as u64);
                            println!("  {:>8}  {}  [photo]", size, node.base().name)
                        }
                    },
                    Err(d) => {
                        let name = degraded_name(d);
                        if !selector_matches(selector.unwrap_or("*"), &name)? {
                            continue;
                        }
                        shown += 1;
                        println!("  {:>8}  {}", "[???]", name)
                    }
                }
            }
        }
        println!("\n  {} item(s)\n", shown);
        return Ok(());
    }

    let current_uid = current_uid.ok_or_else(|| anyhow!("No current directory"))?;

    if let Some(sel) = selector {
        if has_wildcards(sel) {
            let children = list_children(&client, current_uid).await?;
            let mut shown = 0usize;
            let path_display = format!("{}/", current_path.join("/"));

            println!("\n  {}\n", path_display);
            for node in &children {
                if !selector_matches(sel, &node.base().name)? {
                    continue;
                }
                shown += 1;
                match node {
                    Node::Folder(_) | Node::Album(_) => {
                        println!("  {:>8}  {}/", "[DIR]", node.base().name);
                    }
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
            if shown == 0 {
                println!("  (empty)");
            }
            println!("\n  {} item(s)\n", shown);
            return Ok(());
        }
    }

    let (target_uid, target_path) = if let Some(path) = selector {
        resolve_folder_path(&client, current_uid, current_path, root_uid, path).await?
    } else {
        (current_uid, current_path)
    };
    let path_display = format!("{}/", target_path.join("/"));

    println!("\n  {}\n", path_display);

    let children = list_children(&client, target_uid).await?;
    if children.is_empty() {
        println!("  (empty)");
    } else {
        let mut folders: Vec<&Node> = children
            .iter()
            .filter(|n| matches!(n, Node::Folder(_) | Node::Album(_)))
            .collect();
        let mut files: Vec<&Node> = children
            .iter()
            .filter(|n| matches!(n, Node::File(_) | Node::Photo(_)))
            .collect();

        folders.sort_by(|a, b| a.base().name.cmp(&b.base().name));
        files.sort_by(|a, b| a.base().name.cmp(&b.base().name));

        for node in folders {
            println!("  {:>8}  {}/", "[DIR]", node.base().name);
        }
        for node in files {
            match node {
                Node::File(f) => {
                    let size = format_size(f.active_revision.size_on_cloud_storage.max(0) as u64);
                    println!("  {:>8}  {}  ({})", size, f.base.base.name, f.base.media_type);
                }
                Node::Photo(f) => {
                    let size = format_size(f.active_revision.size_on_cloud_storage.max(0) as u64);
                    println!("  {:>8}  {}  [photo]", size, f.base.base.name);
                }
                _ => unreachable!(),
            }
        }
    }
    println!("\n  {} item(s)\n", children.len());
    Ok(())
}

/// `cd <name|..|/>` — navigate the directory tree
pub async fn cd_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: cd <name | .. | />"));
    }
    let target = args[0];

    let (client, current_uid, root_uid, current_path, area) = {
        let s = state.lock().await;
        let client = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated"))?
            .clone();
        let uid = s.get_current_node_uid().cloned();
        let root = s.get_root_node_uid().cloned();
        let path = s.get_current_path().to_vec();
        let area = area_from_path(&path);
        (client, uid, root, path, area)
    };

    let mut new_area = if target.starts_with('/') { Area::Top } else { area };
    let mut new_path = match new_area {
        Area::Top => Vec::<String>::new(),
        _ => current_path,
    };
    let mut new_uid = match new_area {
        Area::MyFiles => current_uid,
        _ => None,
    };

    let segments: Vec<&str> = target.split('/').collect();
    for raw_seg in segments {
        let seg = raw_seg.trim();
        if seg.is_empty() || seg == "." {
            continue;
        }

        match seg {
            ".." => match new_area {
                Area::Top => {}
                Area::Trash => {
                    new_area = Area::Top;
                    new_path.clear();
                    new_uid = None;
                }
                Area::MyFiles => {
                    if new_path.len() <= 1 {
                        new_area = Area::Top;
                        new_path.clear();
                        new_uid = None;
                    } else {
                        let uid = new_uid.clone().ok_or_else(|| anyhow!("No current directory"))?;
                        let node_result = client.get_node(uid).await?;
                        let parent_uid = match &node_result {
                            proton_drive_sdk::utils::PotentialObject::Node(n) => n.base().parent_uid.clone(),
                            proton_drive_sdk::utils::PotentialObject::Degraded(d) => d.parent_uid().cloned(),
                        }
                        .ok_or_else(|| anyhow!("Cannot resolve parent directory"))?;
                        new_uid = Some(parent_uid);
                        new_path.pop();
                    }
                }
            },
            "MyFiles" | "myfiles" | "my_files" => {
                let root = root_uid.clone().ok_or_else(|| anyhow!("Root unknown; please re-login"))?;
                new_area = Area::MyFiles;
                new_path = vec!["MyFiles".to_string()];
                new_uid = Some(root);
            }
            "Trash" | "trash" => {
                new_area = Area::Trash;
                new_path = vec!["Trash".to_string()];
                new_uid = None;
            }
            "Photos" | "photos" => {
                return Err(anyhow!("Photos is not implemented yet"));
            }
            name => match new_area {
                Area::Top => {
                    return Err(anyhow!(
                        "Unknown top-level directory '{}'. Use MyFiles, Trash, or Photos.",
                        name
                    ));
                }
                Area::Trash => {
                    return Err(anyhow!("Trash navigation only supports '.', '..', and top-level targets"));
                }
                Area::MyFiles => {
                    let uid = new_uid.clone().ok_or_else(|| anyhow!("No current directory"))?;
                    let children = list_children(&client, uid).await?;
                    let mut folder_matches: Vec<Node> = Vec::new();
                    for child in children {
                        if !matches!(child, Node::Folder(_) | Node::Album(_)) {
                            continue;
                        }
                        if selector_matches(name, &child.base().name)? {
                            folder_matches.push(child);
                        }
                    }

                    if folder_matches.is_empty() {
                        return Err(anyhow!("Directory '{}' not found", name));
                    }
                    if folder_matches.len() > 1 {
                        let choices = folder_matches
                            .iter()
                            .map(|n| n.base().name.clone())
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(anyhow!(
                            "Pattern '{}' matched multiple directories: {}",
                            name,
                            choices
                        ));
                    }
                    let folder = folder_matches.remove(0);
                    new_uid = Some(folder.uid().clone());
                    new_path.push(folder.base().name.clone());
                }
            },
        }
    }

    let mut s = state.lock().await;
    match new_area {
        Area::MyFiles => s.set_current_node_uid(new_uid.ok_or_else(|| anyhow!("No current directory"))?),
        Area::Top | Area::Trash => s.clear_current_node_uid(),
    }
    s.set_current_path(new_path);
    Ok(())
}
