use std::future::Future;
use std::pin::Pin;

use crate::state::ReplState;
use crate::rusqlite_cache::RusqliteCache;
use crate::app_paths::{resolve_paths, AppDataPaths};
use anyhow::{anyhow, Result};
use glob::glob;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::client::ProtonDriveClient;
use std::sync::Arc;
use parking_lot::Mutex;

use super::helpers::{
    download_progress_bar, find_child_by_name, finish_progress, has_wildcards, list_children,
    node_file_size, progress_callback, resolve_folder_path, selector_matches,
    split_parent_and_leaf, upload_progress_bar,
};

pub async fn hydrate_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: hydrate <remote_path|pattern>"));
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

    let paths = resolve_paths()?;

    for arg in args {
        if has_wildcards(arg) {
            // Wildcard: enumerate the current folder and hydrate all matches.
            let (parent_selector, leaf_selector) = split_parent_and_leaf(arg);
            let target_uid: NodeUid = if parent_selector == "." {
                current_uid.clone().ok_or_else(|| anyhow!("No current directory"))?
            } else {
                let cu = current_uid.clone().ok_or_else(|| anyhow!("No current directory"))?;
                resolve_folder_path(&client, cu, current_path.clone(), root_uid.clone(), parent_selector, Some(&*cache)).await?.0
            };
            let children = list_children(&client, target_uid).await?;
            let matched: Vec<_> = children.into_iter()
                .filter(|n| selector_matches(leaf_selector, &n.base().name).unwrap_or(false))
                .collect();
            if matched.is_empty() {
                println!("No files matched '{}'", arg);
                continue;
            }
            for node in matched {
                hydrate_node(&client, &cache, &paths, node).await?;
            }
        } else {
            let (parent_selector, leaf_selector) = split_parent_and_leaf(arg);
            let target_uid: NodeUid = if parent_selector == "." {
                current_uid.clone().ok_or_else(|| anyhow!("No current directory"))?
            } else {
                let cu = current_uid.clone().ok_or_else(|| anyhow!("No current directory"))?;
                resolve_folder_path(&client, cu, current_path.clone(), root_uid.clone(), parent_selector, Some(&*cache)).await?.0
            };
            let node = find_child_by_name(&client, target_uid, leaf_selector, Some(&*cache)).await?;
            hydrate_node(&client, &cache, &paths, node).await?;
        }
    }

    Ok(())
}

async fn hydrate_node(
    client: &ProtonDriveClient,
    cache: &Arc<RusqliteCache>,
    paths: &AppDataPaths,
    node: proton_drive_sdk::node::Node,
) -> Result<()> {
    use proton_drive_sdk::node::Node;
    match node {
        Node::File(_) | Node::Photo(_) => {
            let node_uid = node.uid().clone();
            let node_name = node.base().name.clone();
            let dest_path = paths.cache_dir.join("files").join(node_uid.link_id.raw());
            let _ = std::fs::create_dir_all(dest_path.parent().unwrap());

            // Skip if already cached on disk.
            if let Ok(Some(cached)) = cache.get_cached_download(&node_uid.volume_id, &node_uid.link_id) {
                if cached.exists() {
                    println!("  '{}' already cached.", node_name);
                    return Ok(());
                }
            }

            let pb = download_progress_bar(&node_name, node_file_size(&node));
            client
                .download_to_file(node_uid.clone(), &dest_path, progress_callback(pb.clone()))
                .await?;
            finish_progress(&pb);
            cache.register_download(&node_uid.volume_id, &node_uid.link_id, &dest_path)?;
            println!("  '{}' cached for offline access.", node_name);
        }
        Node::Folder(_) | Node::Album(_) => {
            let folder_uid = node.uid().clone();
            let folder_name = node.base().name.clone();
            println!("  Hydrating folder '{}'...", folder_name);
            let children = list_children(client, folder_uid).await?;
            let mut count = 0usize;
            for child in children {
                hydrate_node_recursive(client, cache, paths, child, &mut count).await?;
            }
            println!("  Hydrated {} file(s) from '{}'.", count, folder_name);
        }
    }
    Ok(())
}

fn hydrate_node_recursive<'a>(
    client: &'a ProtonDriveClient,
    cache: &'a Arc<RusqliteCache>,
    paths: &'a AppDataPaths,
    node: proton_drive_sdk::node::Node,
    count: &'a mut usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        use proton_drive_sdk::node::Node;
        match node {
            Node::File(_) | Node::Photo(_) => {
                let node_uid = node.uid().clone();
                let node_name = node.base().name.clone();
                let dest = paths.cache_dir.join("files").join(node_uid.link_id.raw());
                let _ = std::fs::create_dir_all(dest.parent().unwrap());
                if let Ok(Some(cached)) = cache.get_cached_download(&node_uid.volume_id, &node_uid.link_id) {
                    if cached.exists() { return Ok(()); }
                }
                let pb = download_progress_bar(&node_name, node_file_size(&node));
                client.download_to_file(node_uid.clone(), &dest, progress_callback(pb.clone())).await?;
                finish_progress(&pb);
                cache.register_download(&node_uid.volume_id, &node_uid.link_id, &dest)?;
                *count += 1;
            }
            Node::Folder(_) | Node::Album(_) => {
                let folder_uid = node.uid().clone();
                let children = list_children(client, folder_uid).await?;
                for child in children {
                    hydrate_node_recursive(client, cache, paths, child, count).await?;
                }
            }
        }
        Ok(())
    })
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
                let pb = download_progress_bar(&node_name, node_file_size(&node));
                client
                    .download_to_file(node_uid.clone(), &local_path, progress_callback(pb.clone()))
                    .await?;
                finish_progress(&pb);
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
                        let pb = download_progress_bar(&node_name, node_file_size(&node));
                        client
                            .download_to_file(node_uid.clone(), &out_path, progress_callback(pb.clone()))
                            .await?;
                        finish_progress(&pb);
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

            match &node {
                Node::File(_) | Node::Photo(_) => {
                    let pb = download_progress_bar(&node_name, node_file_size(&node));
                    client
                        .download_to_file(node_uid, &sub_path, progress_callback(pb.clone()))
                        .await?;
                    finish_progress(&pb);
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

    // Collect local paths — both files and directories are accepted.
    let mut local_paths: Vec<std::path::PathBuf> = Vec::new();
    if local_selector.contains('*') || local_selector.contains('?') {
        for entry in glob(local_selector)? {
            local_paths.push(entry?);
        }
        if local_paths.is_empty() {
            return Err(anyhow!("No local files matched '{}'", local_selector));
        }
    } else {
        let p = std::path::PathBuf::from(local_selector);
        if !p.exists() {
            return Err(anyhow!("Path does not exist: '{}'", local_selector));
        }
        local_paths.push(p);
    }

    let mut total = 0usize;
    for path in &local_paths {
        total += upload_path_recursive(&client, path, target_uid.clone()).await?;
    }
    println!("Uploaded {} item(s).", total);

    Ok(())
}

/// Recursively upload a local file or directory tree to a remote folder.
/// Returns the number of files uploaded.
fn upload_path_recursive<'a>(
    client: &'a ProtonDriveClient,
    local_path: &'a std::path::Path,
    remote_uid: NodeUid,
) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>> {
    Box::pin(async move {
        if local_path.is_file() {
            upload_single_file(client, local_path, remote_uid).await?;
            Ok(1)
        } else if local_path.is_dir() {
            let dir_name = local_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "upload".to_string());
            let sub_uid = get_or_create_remote_folder(client, remote_uid, &dir_name).await?;
            let mut count = 0usize;
            for entry in std::fs::read_dir(local_path)? {
                let entry = entry?;
                count += upload_path_recursive(client, &entry.path(), sub_uid.clone()).await?;
            }
            Ok(count)
        } else {
            Ok(0)
        }
    })
}

async fn upload_single_file(
    client: &ProtonDriveClient,
    path: &std::path::Path,
    remote_uid: NodeUid,
) -> Result<()> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let pb = upload_progress_bar(&name, file_size);
    let result = client
        .upload_file(path, remote_uid.clone(), false, progress_callback(pb.clone()))
        .await;
    pb.set_position(file_size);
    finish_progress(&pb);
    match result {
        Ok(_) => {}
        Err(e) if e.to_string().contains("2500") => {
            // File already exists — update its revision.
            let children = list_children(client, remote_uid.clone()).await?;
            if let Some(existing) = children.iter().find(|n| n.base().name == name) {
                let pb2 = upload_progress_bar(&name, file_size);
                client
                    .upload_file(path, existing.uid().clone(), true, progress_callback(pb2.clone()))
                    .await?;
                pb2.set_position(file_size);
                finish_progress(&pb2);
            }
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Return the UID of a remote folder `name` under `parent_uid`, creating it
/// if it does not yet exist.
async fn get_or_create_remote_folder(
    client: &ProtonDriveClient,
    parent_uid: NodeUid,
    name: &str,
) -> Result<NodeUid> {
    match client.create_folder(parent_uid.clone(), name.to_string(), None).await {
        Ok(f) => Ok(f.base.uid),
        Err(e) if e.to_string().contains("2500") => {
            // Folder already exists — locate its UID in the parent's children.
            let children = list_children(client, parent_uid).await?;
            children
                .iter()
                .find(|n| n.base().name == name)
                .map(|n| n.uid().clone())
                .ok_or_else(|| anyhow!("Remote folder '{}' exists but was not found in listing", name))
        }
        Err(e) => Err(e),
    }
}
