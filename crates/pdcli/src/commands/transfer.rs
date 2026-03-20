use std::future::Future;
use std::pin::Pin;

use crate::state::ReplState;
use anyhow::{anyhow, Result};
use glob::glob;
use proton_drive_sdk::node::Node;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::helpers::{
    expand_local_path, find_child_by_name, has_wildcards, list_children,
    progress_bar_for, progress_callback, resolve_folder_path, selector_matches,
    split_parent_and_leaf,
};

/// Recursively download a folder and its contents
fn download_folder_recursive<'a>(
    client: &'a proton_drive_sdk::client::ProtonDriveClient,
    folder_uid: proton_drive_sdk::node::NodeUid,
    local_path: &'a std::path::Path,
    state: &'a Arc<Mutex<ReplState>>,
) -> Pin<Box<dyn Future<Output = Result<usize>> + 'a>> {
    Box::pin(async move {
        std::fs::create_dir_all(local_path)?;
        let children = list_children(client, folder_uid).await?;
        let mut count = 0;

        for node in children {
            // Check for cancellation
            if state.lock().await.is_cancelled() {
                return Err(anyhow!("Download cancelled"));
            }

            let node_uid = node.uid().clone();
            let node_name = node.base().name.clone();
            
            match &node {
                Node::File(_) | Node::Photo(_) => {
                    let out_path = local_path.join(&node_name);
                    println!("  Downloading '{}' → '{}'", node_name, out_path.display());
                    let pb = progress_bar_for(&node_name, 0);
                    client
                        .download_to_file(
                            node_uid,
                            &out_path,
                            progress_callback(pb.clone()),
                        )
                        .await?;
                    pb.finish_and_clear();
                    count += 1;
                }
                Node::Folder(_) | Node::Album(_) => {
                    let subfolder_path = local_path.join(&node_name);
                    println!("  Creating folder '{}'", subfolder_path.display());
                    let subfolder_count =
                        download_folder_recursive(client, node_uid, &subfolder_path, state)
                            .await?;
                    count += subfolder_count;
                }
            }
        }

        Ok(count)
    })
}

/// `get [-r] <name|pattern> [local_path]` — download a file or folder from the current directory
pub async fn download_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: get [-r] <name|pattern> [local_path]"));
    }

    let mut recursive = false;
    let mut arg_start = 0;

    // Parse -r flag
    for (i, arg) in args.iter().enumerate() {
        if *arg == "-r" {
            recursive = true;
        } else {
            arg_start = i;
            break;
        }
    }

    if arg_start >= args.len() {
        return Err(anyhow!("Usage: get [-r] <name|pattern> [local_path]"));
    }

    if args.len() - arg_start > 2 {
        return Err(anyhow!("Usage: get [-r] <name|pattern> [local_path]"));
    }

    let remote_selector = args[arg_start];
    let local_arg = args.get(arg_start + 1).copied();
    let (client, current_uid, current_path, root_uid) = {
        let s = state.lock().await;
        let client = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let uid = s
            .get_current_node_uid()
            .ok_or_else(|| anyhow!("No current directory"))?
            .clone();
        let current_path = s.get_current_path().to_vec();
        let root_uid = s
            .get_root_node_uid()
            .ok_or_else(|| anyhow!("Root unknown; please re-login"))?
            .clone();
        (client, uid, current_path, root_uid)
    };

    let (parent_selector, leaf_selector) = split_parent_and_leaf(remote_selector);
    let target_uid = if parent_selector == "." {
        current_uid.clone()
    } else {
        resolve_folder_path(
            &client,
            current_uid.clone(),
            current_path,
            root_uid,
            parent_selector,
        )
        .await?
        .0
    };

    if !has_wildcards(leaf_selector) {
        let default_name = if leaf_selector.is_empty() { remote_selector } else { leaf_selector };
        let local_path = expand_local_path(local_arg.unwrap_or(default_name));
        let node = find_child_by_name(&client, target_uid, leaf_selector).await?;
        let node_uid = node.uid().clone();
        let node_name = node.base().name.clone();

        match &node {
            Node::File(_) | Node::Photo(_) => {
                println!("Downloading '{}' → '{}'", node_name, local_path.display());
                let pb = progress_bar_for(&node_name, 0);
                client
                    .download_to_file(node_uid, &local_path, progress_callback(pb.clone()))
                    .await?;
                pb.finish_and_clear();
                println!("Saved to: {}", local_path.display());
            }
            Node::Folder(_) | Node::Album(_) => {
                if recursive {
                    println!("Downloading folder '{}' recursively...", node_name);
                    let count = download_folder_recursive(&client, node_uid, &local_path, state)
                        .await?;
                    println!("Downloaded {} item(s) to '{}'", count, local_path.display());
                } else {
                    return Err(anyhow!(
                        "'{}' is a folder. Use 'get -r {}' to download it recursively.",
                        node_name,
                        leaf_selector
                    ));
                }
            }
        }

        return Ok(());
    }

    let children = list_children(&client, target_uid).await?;
    let mut matches: Vec<Node> = children
        .into_iter()
        .filter(|n| {
            let matches_pattern = selector_matches(leaf_selector, &n.base().name).unwrap_or(false);
            matches_pattern
                && (recursive
                    || matches!(n, Node::File(_) | Node::Photo(_)))
        })
        .collect();

    if matches.is_empty() {
        let type_str = if recursive { "files or folders" } else { "files" };
        return Err(anyhow!(
            "No {} matched pattern '{}'",
            type_str,
            remote_selector
        ));
    }

    let destination = expand_local_path(local_arg.unwrap_or("."));
    if !destination.is_dir() {
        return Err(anyhow!(
            "When using wildcards with get, local_path must be an existing directory"
        ));
    }

    matches.sort_by(|a, b| a.base().name.cmp(&b.base().name));
    let mut total_count = 0;

    for node in matches {
        // Check for cancellation
        if state.lock().await.is_cancelled() {
            return Err(anyhow!("Download cancelled"));
        }

        let node_uid = node.uid().clone();
        let node_name = node.base().name.clone();
        
        match &node {
            Node::File(_) | Node::Photo(_) => {
                let out_path = destination.join(&node_name);
                println!("Downloading '{}' → '{}'", node_name, out_path.display());
                let pb = progress_bar_for(&node_name, 0);
                client
                    .download_to_file(node_uid, &out_path, progress_callback(pb.clone()))
                    .await?;
                pb.finish_and_clear();
                println!("Saved to: {}", out_path.display());
                total_count += 1;
            }
            Node::Folder(_) | Node::Album(_) => {
                if recursive {
                    let subfolder_path = destination.join(&node_name);
                    println!("Creating folder '{}'", subfolder_path.display());
                    let subfolder_count =
                        download_folder_recursive(&client, node_uid, &subfolder_path, state)
                            .await?;
                    total_count += subfolder_count;
                }
            }
        }
    }

    println!("Downloaded {} item(s)", total_count);
    return Ok(());
}

/// `put <local_path> [remote_path]` — upload a local file into the current directory
pub async fn upload_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: put <local_path|pattern> ... [remote_dest]"));
    }

    // If the last argument is not a local path and there are at least 2 args,
    // treat it as the remote destination (name or path).
    let (local_args, remote_dest) = {
        let last = args.last().unwrap();
        let last_local = expand_local_path(last);
        if args.len() >= 2 && !has_wildcards(last) && !last_local.exists() {
            (&args[..args.len() - 1], Some(*last))
        } else {
            (args, None)
        }
    };

    let (client, current_uid, current_path, root_uid) = {
        let s = state.lock().await;
        let client = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let uid = s
            .get_current_node_uid()
            .ok_or_else(|| anyhow!("No current directory"))?
            .clone();
        let root_uid = s
            .get_root_node_uid()
            .ok_or_else(|| anyhow!("Root unknown; please re-login"))?
            .clone();
        (client, uid, s.get_current_path().to_vec(), root_uid)
    };

    // Resolve remote destination folder and optional leaf name
    let (upload_folder_uid, remote_leaf_name) = if let Some(dest) = remote_dest {
        let (parent, leaf) = split_parent_and_leaf(dest);
        let folder_uid = if parent == "." {
            current_uid.clone()
        } else {
            resolve_folder_path(&client, current_uid.clone(), current_path.clone(), root_uid.clone(), parent)
                .await?
                .0
        };
        // If leaf has an extension or doesn't look like a folder, treat as rename target
        let leaf_name = if leaf.is_empty() { None } else { Some(leaf.to_string()) };
        (folder_uid, leaf_name)
    } else {
        (current_uid.clone(), None)
    };

    let mut local_files = Vec::new();
    for arg in local_args {
        if has_wildcards(arg) {
            let pattern = expand_local_path(arg);
            let pattern_str = pattern.to_string_lossy().into_owned();
            for entry in glob(&pattern_str)? {
                let path = entry?;
                if path.is_file() {
                    local_files.push(path);
                }
            }
        } else {
            let path = expand_local_path(arg);
            if path.is_file() {
                local_files.push(path);
            } else if path.is_dir() {
                return Err(anyhow!("'{}' is a directory, not a file.", path.display()));
            } else {
                return Err(anyhow!("'{}' does not exist. Check the path and try again.", path.display()));
            }
        }
    }

    if local_files.is_empty() {
        return Err(anyhow!("Nothing matched"));
    }

    local_files.sort();
    local_files.dedup();

    // If a leaf rename is specified but multiple files, error clearly
    if remote_leaf_name.is_some() && local_files.len() > 1 {
        return Err(anyhow!(
            "Cannot upload multiple files to a single remote name '{}'. Specify a folder as destination instead.",
            remote_leaf_name.unwrap()
        ));
    }

    for local_path in &local_files {
        // Check for cancellation
        if state.lock().await.is_cancelled() {
            return Err(anyhow!("Upload cancelled"));
        }

        let filename = local_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("upload");
        let file_size = local_path.metadata().map(|m| m.len()).unwrap_or(0);
        let display_dest = remote_leaf_name.as_deref().unwrap_or(filename);
        println!("Uploading '{}' → '{}'", local_path.display(), display_dest);
        let pb = progress_bar_for(filename, file_size);
        client
            .upload_file(local_path, upload_folder_uid.clone(), false, progress_callback(pb.clone()))
            .await?;
        pb.finish_and_clear();
        println!("✓ Uploaded");

        // Rename if a leaf name was supplied
        if let Some(ref name) = remote_leaf_name {
            if name != filename {
                // Find the just-uploaded node by its original filename
                let children = list_children(&client, upload_folder_uid.clone()).await?;
                if let Some(node) = children.iter().find(|n| n.base().name == filename) {
                    client
                        .rename_node(node.uid().clone(), name.clone(), None)
                        .await?;
                }
            }
        }
    }

    println!("Uploaded {} item(s)", local_files.len());
    Ok(())
}
