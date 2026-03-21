use std::future::Future;
use std::pin::Pin;

use crate::state::ReplState;
use crate::rusqlite_cache::RusqliteCache;
use crate::app_paths::resolve_paths;
use anyhow::{anyhow, Result};
use glob::glob;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::client::ProtonDriveClient;
use std::sync::Arc;
use parking_lot::Mutex;

use super::helpers::{
    find_child_by_name, has_wildcards, list_children, progress_bar_for, progress_callback,
    resolve_folder_path, selector_matches, split_parent_and_leaf,
};

pub async fn hydrate_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: hydrate <remote_path>"));
    }

    let (client, current_uid, current_path, root_uid, cache) = {
        let s = state.lock();
        let client = s.get_client().ok_or_else(|| anyhow!("Not authenticated"))?.clone();
        let uid = s.get_current_node_uid().cloned();
        let path = s.get_current_path().to_vec();
        let root = s.get_root_node_uid().cloned().ok_or_else(|| anyhow!("Root unknown"))?;
        let cache = s.get_cache().ok_or_else(|| anyhow!("Cache unknown"))?;
        (client, uid, path, root, cache)
    };

    for arg in args {
        let (parent_selector, leaf_selector) = split_parent_and_leaf(arg);
        let target_uid: NodeUid = if parent_selector == "." {
            current_uid.clone().ok_or_else(|| anyhow!("No current directory"))?
        } else {
            let cu = current_uid.clone().ok_or_else(|| anyhow!("No current directory"))?;
            resolve_folder_path(&client, cu, current_path.clone(), root_uid.clone(), parent_selector, Some(&*cache)).await?.0
        };

        let node = find_child_by_name(&client, target_uid, leaf_selector, Some(&*cache)).await?;
        let node_uid = node.uid().clone();
        let node_name = node.base().name.clone();

        match &node {
            Node::File(_) | Node::Photo(_) => {
                let paths = resolve_paths()?;
                let dest_path = paths.cache_dir.join("files").join(node_uid.link_id.raw());
                let _ = std::fs::create_dir_all(dest_path.parent().unwrap());
                
                println!("Hydrating '{}'...", node_name);
                let pb = progress_bar_for(&node_name, 0);
                client
                    .download_to_file(node_uid.clone(), &dest_path, progress_callback(pb.clone()))
                    .await?;
                pb.finish_and_clear();
                
                cache.register_download(&node_uid.volume_id, &node_uid.link_id, &dest_path)?;
                println!("'{}' is now available offline.", node_name);
            }
            _ => println!("'{}' is a directory, hydration not needed.", node_name),
        }
    }

    Ok(())
}

pub async fn download_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: get <remote> [local]"));
    }

    let remote_selector = args[0];
    let local_arg = args.get(1).copied();

    let (client, current_uid, current_path, root_uid) = {
        let s = state.lock();
        let client = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let uid = s.get_current_node_uid().cloned();
        let p = s.get_current_path().to_vec();
        let root = s.get_root_node_uid().cloned().expect("root exists");
        (client, uid, p, root)
    };

    let cache_path = resolve_paths()?.cache_dir.join("drive_cache.db");
    let cache = RusqliteCache::new(&cache_path)?;

    let (parent_selector, leaf_selector) = split_parent_and_leaf(remote_selector);
    let target_uid: NodeUid = if parent_selector == "." {
        current_uid.ok_or_else(|| anyhow!("No current directory"))?
    } else {
        let tu = current_uid.ok_or_else(|| anyhow!("No current directory"))?;
        resolve_folder_path(
            &client,
            tu,
            current_path,
            root_uid,
            parent_selector,
            Some(&cache),
        )
        .await?
        .0
    };

    if !has_wildcards(leaf_selector) {
        let default_name = if leaf_selector.is_empty() { remote_selector } else { leaf_selector };
        let local_path = expand_local_path(local_arg.unwrap_or(default_name));
        let node = find_child_by_name(&client, target_uid, leaf_selector, Some(&cache)).await?;
        let node_uid = node.uid().clone();
        let node_name = node.base().name.clone();

        match &node {
            Node::File(_) | Node::Photo(_) => {
                if let Ok(Some(cached_path)) = cache.get_cached_download(&node_uid.volume_id, &node_uid.link_id) {
                    if cached_path.exists() {
                        println!("Using cached file: {}", cached_path.display());
                        if local_path != cached_path {
                            std::fs::copy(&cached_path, &local_path)?;
                        }
                        return Ok(());
                    }
                }

                println!("Downloading '{}' → '{}'", node_name, local_path.display());
                let pb = progress_bar_for(&node_name, 0);
                client
                    .download_to_file(node_uid.clone(), &local_path, progress_callback(pb.clone()))
                    .await?;
                pb.finish_and_clear();
                cache.register_download(&node_uid.volume_id, &node_uid.link_id, &local_path)?;
            }
            Node::Folder(_) | Node::Album(_) => {
                println!("Downloading folder '{}' recursively...", node_name);
                let count = download_folder_recursive(&client, node_uid, &local_path, state).await?;
                println!("Downloaded {} items from '{}'", count, node_name);
            }
        }
    } else {
        let destination = expand_local_path(local_arg.unwrap_or("."));
        let children = list_children(&client, target_uid).await?;
        let mut total_count = 0usize;

        for node in children {
            if selector_matches(leaf_selector, &node.base().name)? {
                let node_uid = node.uid().clone();
                let node_name = node.base().name.clone();

                match &node {
                    Node::File(_) | Node::Photo(_) => {
                        let out_path = destination.join(&node_name);
                        println!("Downloading '{}' → '{}'", node_name, out_path.display());
                        let pb = progress_bar_for(&node_name, 0);
                        client
                            .download_to_file(node_uid.clone(), &out_path, progress_callback(pb.clone()))
                            .await?;
                        pb.finish_and_clear();
                        cache.register_download(&node_uid.volume_id, &node_uid.link_id, &out_path)?;
                        total_count += 1;
                    }
                    _ => {}
                }
            }
        }
        println!("Downloaded {} item(s)", total_count);
    }

    Ok(())
}

fn expand_local_path(path: &str) -> std::path::PathBuf {
    if path.starts_with("~/") {
        if let Some(home) = platform_dirs::UserDirs::new().map(|u| u.desktop_dir.parent().unwrap().to_path_buf()) {
            return home.join(&path[2..]);
        }
    }
    std::path::PathBuf::from(path)
}

fn download_folder_recursive<'a>(
    client: &'a ProtonDriveClient,
    folder_uid: NodeUid,
    local_path: &'a std::path::Path,
    state: &'a Arc<Mutex<ReplState>>,
) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>> {
    Box::pin(async move {
        std::fs::create_dir_all(local_path)?;
        let children = list_children(client, folder_uid).await?;
        let mut count = 0usize;

        for node in children {
            let node_uid = node.uid().clone();
            let node_name = node.base().name.clone();
            let sub_path = local_path.join(&node_name);

            match node {
                Node::File(_) | Node::Photo(_) => {
                    let pb = progress_bar_for(&node_name, 0);
                    client
                        .download_to_file(node_uid, &sub_path, progress_callback(pb.clone()))
                        .await?;
                    pb.finish_and_clear();
                    count += 1;
                }
                Node::Folder(_) | Node::Album(_) => {
                    count += download_folder_recursive(client, node_uid, &sub_path, state).await?;
                }
            }
        }
        Ok(count)
    })
}

pub async fn upload_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: put <local> [remote]"));
    }

    let local_selector = args[0];
    let remote_arg = args.get(1).copied();

    let (client, current_uid, current_path, root_uid) = {
        let s = state.lock();
        let client = s.get_client().ok_or_else(|| anyhow!("Not authenticated"))?.clone();
        let uid = s.get_current_node_uid().cloned();
        let path = s.get_current_path().to_vec();
        let root = s.get_root_node_uid().cloned().expect("root exists");
        (client, uid, path, root)
    };

    let target_uid: NodeUid = if let Some(dst) = remote_arg {
        let cu = current_uid.clone().ok_or_else(|| anyhow!("No current directory"))?;
        resolve_folder_path(&client, cu, current_path.clone(), root_uid.clone(), dst, None).await?.0
    } else {
        current_uid.ok_or_else(|| anyhow!("No current directory"))?
    };

    let mut local_files = Vec::new();
    if local_selector.contains('*') || local_selector.contains('?') {
        for entry in glob(local_selector)? {
            let path = entry?;
            if path.is_file() {
                local_files.push(path);
            }
        }
    } else {
        local_files.push(std::path::PathBuf::from(local_selector));
    }

    if local_files.is_empty() {
        return Err(anyhow!("No local files matched '{}'", local_selector));
    }

    for path in &local_files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        println!("Uploading '{}'...", name);
        let pb = progress_bar_for(&name, 0);
        
        let result = client.upload_file(path, target_uid.clone(), false, progress_callback(pb.clone())).await;
        pb.finish_and_clear();
        
        match result {
            Ok(_) => println!("Uploaded '{}'", name),
            Err(e) if format!("{}", e).contains("2500") => {
                println!("File '{}' already exists. Updating revision...", name);
                let children = list_children(&client, target_uid.clone()).await?;
                if let Some(existing) = children.iter().find(|n| n.base().name == name) {
                    client.upload_file(path, existing.uid().clone(), true, progress_callback(progress_bar_for(&name, 0))).await?;
                    println!("Updated revision for '{}'", name);
                }
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}
