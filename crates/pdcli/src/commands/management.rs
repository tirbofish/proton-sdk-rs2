use crate::state::ReplState;
use anyhow::{anyhow, Result};
use dialoguer::Confirm;
use glob::Pattern;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::utils::PotentialObject;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::helpers::{
    area_from_path, degraded_name, find_child_by_name, format_size, get_available_name,
    has_wildcards, list_children, resolve_folder_path, selector_matches, split_parent_and_leaf,
    Area,
};

pub async fn mkdir_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: mkdir <folder_name>"));
    }
    let name = args[0].to_string();
    
    let (client, parent_uid) = {
        let s = state.lock().await;
        let c = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let u = s
            .get_current_node_uid()
            .ok_or_else(|| anyhow!("No current directory"))?
            .clone();
        (c, u)
    };

    let new_folder = client.create_folder(parent_uid, name.clone(), None).await?;
    println!("Created: {} ({})", name, new_folder.base.uid);
    Ok(())
}

/// Check if an error is a 2500 API conflict error
fn is_conflict_error_2500(error: &anyhow::Error) -> bool {
    format!("{}", error).contains("2500")
}

async fn handle_move_conflict(
    existing: &Node,
    _moving_is_folder: bool,
) -> Result<MoveConflictAction> {
    let _existing_is_folder = matches!(existing, Node::Folder(_) | Node::Album(_));
    let existing_name = existing.base().name.clone();
    let existing_size = match existing {
        Node::File(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
        Node::Photo(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
        Node::Folder(_) | Node::Album(_) => "[DIR]".to_string(),
    };

    loop {
        println!("\nConflict: Destination '{}' already exists", existing_name);
        println!("\n  Options:");
        println!("    0 - Replace the file");
        println!("    1 - Compare file stats");
        println!("    2 - Change name and move");
        println!("    3 - Skip");
        println!("\n  Enter choice (0-3): ");

        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err() {
            return Ok(MoveConflictAction::Skip);
        }

        match input.trim() {
            "0" => return Ok(MoveConflictAction::Replace),
            "1" => {
                println!("\n  Existing: {}  {}", existing_size, existing_name);
                return Ok(MoveConflictAction::CompareStats);
            }
            "2" => return Ok(MoveConflictAction::Rename),
            "3" => return Ok(MoveConflictAction::Skip),
            _ => println!("Invalid choice. Please enter 0-3."),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MoveConflictAction {
    Replace,
    CompareStats,
    Rename,
    Skip,
}

pub async fn move_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.len() < 2 {
        return Err(anyhow!("Usage: mv [--force] <src> <dst>"));
    }

    let mut force = false;
    let mut arg_start = 0;

    for (i, arg) in args.iter().enumerate() {
        match *arg {
            "-f" | "--force" => force = true,
            _ => {
                arg_start = i;
                break;
            }
        }
    }

    if args.len() - arg_start < 2 {
        return Err(anyhow!("Usage: mv [--force] <src> <dst>"));
    }

    let src = args[arg_start];
    let dst = args[arg_start + 1];

    let (client, current_uid, root_uid, current_path) = {
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
        (client, uid, root_uid, s.get_current_path().to_vec())
    };

    if has_wildcards(src) {
        let pattern = Pattern::new(src)?;
        let children = list_children(&client, current_uid.clone()).await?;
        let matches: Vec<Node> = children
            .into_iter()
            .filter(|n| pattern.matches(&n.base().name))
            .collect();
        if matches.is_empty() {
            return Err(anyhow!("No files matched pattern '{}'", src));
        }

        let (dst_uid, _) = resolve_folder_path(
            &client,
            current_uid,
            current_path,
            root_uid,
            dst,
        )
        .await?;

        let uids = matches.iter().map(|n| n.uid().clone()).collect::<Vec<_>>();
        client.move_nodes(uids, dst_uid).await?;
        println!("Moved {} item(s) to '{}'", matches.len(), dst);
        return Ok(());
    }

    // Resolve the source — it may be a relative path like "tests/BrainStem.glb"
    let (src_parent, src_leaf) = split_parent_and_leaf(src);
    let src_folder_uid = if src_parent == "." {
        current_uid.clone()
    } else {
        resolve_folder_path(
            &client,
            current_uid.clone(),
            current_path.clone(),
            root_uid.clone(),
            src_parent,
        )
        .await?
        .0
    };
    let node = find_child_by_name(&client, src_folder_uid.clone(), src_leaf).await?;

    if dst.contains('/') {
        let (folder_part, maybe_name) = match dst.rsplit_once('/') {
            Some((f, n)) => (f, n),
            None => ("", dst),
        };
        let target_folder_path = if folder_part.is_empty() { "." } else { folder_part };
        let (target_folder_uid, _) = resolve_folder_path(
            &client,
            current_uid.clone(),
            current_path.clone(),
            root_uid.clone(),
            target_folder_path,
        )
        .await?;

        if target_folder_uid != *node.base().parent_uid.as_ref().unwrap_or(&target_folder_uid) {
            match client
                .move_nodes(vec![node.uid().clone()], target_folder_uid.clone())
                .await
            {
                Ok(_) => {}
                Err(e) if is_conflict_error_2500(&e) => {
                    // File exists in destination, show conflict dialog
                    let target_children = list_children(&client, target_folder_uid.clone()).await?;
                    if let Some(existing) = target_children.iter().find(|n| n.base().name == node.base().name) {
                        println!("\n(Conflict detected: destination already has '{}')", node.base().name);
                        match handle_move_conflict(existing, matches!(node, Node::Folder(_) | Node::Album(_))).await? {
                            MoveConflictAction::Replace => {
                                // continue with the move as-is
                            }
                            MoveConflictAction::CompareStats => {
                                let existing_size = match existing {
                                    Node::File(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
                                    Node::Photo(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
                                    Node::Folder(_) | Node::Album(_) => "[DIR]".to_string(),
                                };
                                let moving_size = match &node {
                                    Node::File(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
                                    Node::Photo(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
                                    Node::Folder(_) | Node::Album(_) => "[DIR]".to_string(),
                                };
                                println!("\n  Existing:  {}  {}", existing_size, existing.base().name);
                                println!("  Moving:    {}  {}", moving_size, node.base().name);
                                println!("  Use 'mv --force' to replace or pick a different destination.");
                                return Ok(());
                            }
                            MoveConflictAction::Rename => {
                                let new_name = get_available_name(&target_children, &node.base().name)?;
                                client
                                    .rename_node(node.uid().clone(), new_name.clone(), None)
                                    .await?;
                                client
                                    .move_nodes(vec![node.uid().clone()], target_folder_uid.clone())
                                    .await?;
                                println!("Moved '{}' → '{}/{}' (conflict resolved)", src, target_folder_path, new_name);
                                return Ok(());
                            }
                            MoveConflictAction::Skip => return Ok(()),
                        }
                    } else {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }

        if !maybe_name.is_empty() {
            let mut final_name = maybe_name.to_string();
            let mut target_children = list_children(&client, target_folder_uid.clone()).await?;
            if let Some(existing) = target_children.iter().find(|n| n.base().name == maybe_name) {
                if force {
                    // fall through to rename below
                } else {
                    match handle_move_conflict(existing, matches!(node, Node::Folder(_) | Node::Album(_))).await? {
                        MoveConflictAction::Replace => {}
                        MoveConflictAction::CompareStats => {
                            let moving_size = match &node {
                                Node::File(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
                                Node::Photo(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
                                Node::Folder(_) | Node::Album(_) => "[DIR]".to_string(),
                            };
                            println!("\n  Moving:   {}  {}", moving_size, node.base().name);
                            println!("  Use 'mv --force' to replace or pick a different destination.");
                            return Ok(());
                        }
                        MoveConflictAction::Rename => {
                            final_name = get_available_name(&target_children, maybe_name)?;
                        }
                        MoveConflictAction::Skip => return Ok(()),
                    }
                }
            }

            // Try to rename; if we get a 2500 error, handle it
            match client
                .rename_node(node.uid().clone(), final_name.clone(), None)
                .await
            {
                Ok(_) => {}
                Err(e) if is_conflict_error_2500(&e) => {
                    // Conflict detected by API, refresh children and show dialog
                    target_children = list_children(&client, target_folder_uid.clone()).await?;
                    if let Some(existing) = target_children.iter().find(|n| n.base().name == final_name) {
                        println!("\n(Conflict detected during move)");
                        match handle_move_conflict(existing, matches!(node, Node::Folder(_) | Node::Album(_))).await? {
                            MoveConflictAction::Replace => {
                                client
                                    .rename_node(node.uid().clone(), final_name.clone(), None)
                                    .await?;
                            }
                            MoveConflictAction::CompareStats => {
                                let moving_size = match &node {
                                    Node::File(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
                                    Node::Photo(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
                                    Node::Folder(_) | Node::Album(_) => "[DIR]".to_string(),
                                };
                                println!("\n  Moving:   {}  {}", moving_size, node.base().name);
                                println!("  Use 'mv --force' to replace or pick a different destination.");
                                return Ok(());
                            }
                            MoveConflictAction::Rename => {
                                let new_name = get_available_name(&target_children, &maybe_name)?;
                                client
                                    .rename_node(node.uid().clone(), new_name.clone(), None)
                                    .await?;
                            }
                            MoveConflictAction::Skip => return Ok(()),
                        }
                    } else {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        println!("Moved '{}' → '{}'", src, dst);
        return Ok(());
    }

    if let Ok(folder) = find_child_by_name(&client, current_uid.clone(), dst).await {
        if matches!(folder, Node::Folder(_) | Node::Album(_)) {
            client
                .move_nodes(vec![node.uid().clone()], folder.uid().clone())
                .await?;
            println!("Moved '{}' → '{}/'", src, dst);
            return Ok(());
        }
    }

    // If the source was in a subdirectory, bring it into the current directory first
    if src_folder_uid != current_uid {
        client
            .move_nodes(vec![node.uid().clone()], current_uid.clone())
            .await?;
    }

    let target_children = list_children(&client, current_uid.clone()).await?;
    let mut final_name = dst.to_string();
    if let Some(existing) = target_children.iter().find(|n| n.base().name == dst) {
        if force {
            // fall through to rename below
        } else {
            match handle_move_conflict(existing, matches!(node, Node::Folder(_) | Node::Album(_))).await? {
                MoveConflictAction::Replace => {}
                MoveConflictAction::CompareStats => {
                    let moving_size = match &node {
                        Node::File(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
                        Node::Photo(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
                        Node::Folder(_) | Node::Album(_) => "[DIR]".to_string(),
                    };
                    println!("\n  Moving:   {}  {}", moving_size, node.base().name);
                    println!("  Use 'mv --force' to replace or pick a different destination.");
                    return Ok(());
                }
                MoveConflictAction::Rename => {
                    final_name = get_available_name(&target_children, dst)?;
                }
                MoveConflictAction::Skip => return Ok(()),
            }
        }
    }

    // Try to rename; if we get a 2500 error, handle it
    match client
        .rename_node(node.uid().clone(), final_name.clone(), None)
        .await
    {
        Ok(_) => {}
        Err(e) if is_conflict_error_2500(&e) => {
            // Conflict detected by API, refresh children and show dialog
            let refreshed_children = list_children(&client, current_uid.clone()).await?;
            if let Some(existing) = refreshed_children.iter().find(|n| n.base().name == final_name) {
                println!("\n(Conflict detected during rename)");
                match handle_move_conflict(existing, matches!(node, Node::Folder(_) | Node::Album(_))).await? {
                    MoveConflictAction::Replace => {
                        client
                            .rename_node(node.uid().clone(), final_name.clone(), None)
                            .await?;
                    }
                    MoveConflictAction::CompareStats => {
                        let moving_size = match &node {
                            Node::File(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
                            Node::Photo(f) => format_size(f.active_revision.size_on_cloud_storage.max(0) as u64),
                            Node::Folder(_) | Node::Album(_) => "[DIR]".to_string(),
                        };
                        println!("\n  Moving:   {}  {}", moving_size, node.base().name);
                        println!("  Use 'mv --force' to replace or pick a different destination.");
                        return Ok(());
                    }
                    MoveConflictAction::Rename => {
                        let new_name = get_available_name(&refreshed_children, &dst)?;
                        client
                            .rename_node(node.uid().clone(), new_name.clone(), None)
                            .await?;
                        final_name = new_name;
                    }
                    MoveConflictAction::Skip => return Ok(()),
                }
            } else {
                return Err(e);
            }
        }
        Err(e) => return Err(e),
    }

    if final_name == dst {
        println!("Renamed '{}' → '{}'", src, dst);
    } else {
        println!("Renamed '{}' → '{}' (conflict resolved)", src, final_name);
    }
    Ok(())
}

pub async fn remove_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: rm <path|pattern> ..."));
    }

    let mut recursive = false;
    let mut force = false;
    let mut targets: Vec<&str> = Vec::new();
    for arg in args {
        match *arg {
            "-r" => recursive = true,
            "-f" => force = true,
            "-rf" | "-fr" => {
                recursive = true;
                force = true;
            }
            _ => targets.push(arg),
        }
    }

    if targets.is_empty() {
        return Err(anyhow!("Usage: rm <path|pattern> ..."));
    }

    let (client, current_uid, current_path, root_uid) = {
        let s = state.lock().await;
        let client = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let root_uid = s
            .get_root_node_uid()
            .ok_or_else(|| anyhow!("Root unknown; please re-login"))?
            .clone();
        (
            client,
            s.get_current_node_uid().cloned(),
            s.get_current_path().to_vec(),
            root_uid,
        )
    };

    let area = area_from_path(&current_path);

    if area == Area::Trash {
        return Err(anyhow!("Use 'drop <name|pattern>' to permanently delete items in Trash"));
    }
    if area != Area::MyFiles {
        return Err(anyhow!("rm is only available in MyFiles or Trash"));
    }

    let current_uid = current_uid.ok_or_else(|| anyhow!("No current directory"))?;
    let mut to_trash = Vec::new();

    for target in targets {
        let (parent_selector, leaf_selector) = split_parent_and_leaf(target);
        let target_uid = if parent_selector == "." {
            current_uid.clone()
        } else {
            resolve_folder_path(
                &client,
                current_uid.clone(),
                current_path.clone(),
                root_uid.clone(),
                parent_selector,
            )
            .await?
            .0
        };
        let children = list_children(&client, target_uid.clone()).await?;

        if has_wildcards(leaf_selector) {
            for node in children
                .iter()
                .filter(|n| selector_matches(leaf_selector, &n.base().name).unwrap_or(false))
            {
                let _ = recursive;
                to_trash.push(node.uid().clone());
            }
            continue;
        }

        match children.into_iter().find(|n| n.base().name == leaf_selector) {
            Some(node) => to_trash.push(node.uid().clone()),
            None if force => {}
            None => {
                return Err(anyhow!(
                    "'{}' not found in '{}'",
                    leaf_selector,
                    parent_selector
                ));
            }
        }
    }

    if to_trash.is_empty() {
        if !force {
            return Err(anyhow!("Nothing matched"));
        }
        return Ok(());
    }

    let mut seen = HashSet::new();
    to_trash.retain(|uid| seen.insert(uid.clone()));

    let result = client.trash_nodes(to_trash.clone()).await?;
    let mut failed = 0usize;
    for uid in &to_trash {
        if let Some(Err(err)) = result.get(uid) {
            failed += 1;
            match client.get_node(uid.clone()).await {
                Ok(PotentialObject::Node(node)) => {
                    eprintln!(
                        "Failed to trash '{}'(uid={}): {}",
                        node.base().name,
                        uid,
                        err
                    );
                }
                _ => {
                    eprintln!("Failed to trash uid={}: {}", uid, err);
                }
            }
        }
    }

    if failed > 0 {
        return Err(anyhow!("{} item(s) failed to trash", failed));
    }

    println!("Trashed {} item(s)", to_trash.len());
    Ok(())
}

pub async fn drop_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: drop <name|pattern> ..."));
    }

    let (client, area) = {
        let s = state.lock().await;
        let client = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let area = area_from_path(s.get_current_path());
        (client, area)
    };

    if area != Area::Trash {
        return Err(anyhow!("drop is only available in Trash"));
    }

    let items = client.enumerate_trash().await?;
    let mut to_delete = Vec::new();
    let mut names_by_uid: HashMap<NodeUid, String> = HashMap::new();

    for selector in args {
        let (parent_selector, leaf_selector) = split_parent_and_leaf(selector);
        if parent_selector != "." && parent_selector != "/" && parent_selector != "Trash" {
            return Err(anyhow!(
                "Invalid Trash selector '{}': only current Trash scope is supported",
                selector
            ));
        }

        for item in &items {
            match item {
                Ok(node) => {
                    if selector_matches(leaf_selector, &node.base().name)? {
                        let uid = node.uid().clone();
                        to_delete.push(uid.clone());
                        names_by_uid.entry(uid).or_insert_with(|| node.base().name.clone());
                    }
                }
                Err(d) => {
                    let name = degraded_name(d);
                    if selector_matches(leaf_selector, &name)? {
                        let uid = d.uid().clone();
                        to_delete.push(uid.clone());
                        names_by_uid.entry(uid).or_insert(name);
                    }
                }
            }
        }
    }

    let mut seen = HashSet::new();
    to_delete.retain(|uid| seen.insert(uid.clone()));
    if to_delete.is_empty() {
        return Err(anyhow!("Nothing matched"));
    }

    println!(
        "Are you sure you want to permanently delete {} item(s) from Trash?",
        to_delete.len()
    );
    println!("You cannot undo this action.");

    let confirmed = Confirm::new()
        .with_prompt("Continue")
        .default(false)
        .interact()?;

    if !confirmed {
        println!("Cancelled.");
        return Ok(());
    }

    let result = client.delete_nodes_from_trash(to_delete.clone()).await?;
    let mut failed = 0usize;
    for uid in &to_delete {
        if let Some(Err(err)) = result.get(uid) {
            failed += 1;
            let name = names_by_uid
                .get(uid)
                .map(String::as_str)
                .unwrap_or("<unknown>");
            eprintln!("Failed to drop '{}'(uid={}): {}", name, uid, err);
        }
    }

    if failed > 0 {
        return Err(anyhow!("{} item(s) failed to drop", failed));
    }

    println!("Dropped {} item(s)", to_delete.len());
    Ok(())
}

/// `stat <name>` — display metadata for a node in the current directory
pub async fn stat_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: stat <name|pattern> ..."));
    }
    
    let (client, current_uid) = {
        let s = state.lock().await;
        let c = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let u = s
            .get_current_node_uid()
            .ok_or_else(|| anyhow!("No current directory"))?
            .clone();
        (c, u)
    };

    let children = list_children(&client, current_uid.clone()).await?;
    let mut matched: Vec<Node> = Vec::new();
    for selector in args {
        let (parent_selector, leaf_selector) = split_parent_and_leaf(selector);
        let target_uid = if parent_selector == "." {
            current_uid.clone()
        } else {
            let s = state.lock().await;
            let root_uid = s
                .get_root_node_uid()
                .ok_or_else(|| anyhow!("Root unknown; please re-login"))?
                .clone();
            drop(s);
            let (_, current_path) = {
                let s = state.lock().await;
                (s.get_current_node_uid().cloned(), s.get_current_path().to_vec())
            };
            let (uid, _) = resolve_folder_path(
                &client,
                current_uid.clone(),
                current_path,
                root_uid,
                parent_selector,
            )
            .await?;
            uid
        };

        let target_children = if parent_selector == "." {
            children.clone()
        } else {
            list_children(&client, target_uid).await?
        };
        for child in &target_children {
            if selector_matches(leaf_selector, &child.base().name)? {
                matched.push(child.clone());
            }
        }
    }

    let mut seen = HashSet::new();
    matched.retain(|node| seen.insert(node.uid().clone()));
    if matched.is_empty() {
        return Err(anyhow!("Nothing matched"));
    }

    for (idx, node) in matched.iter().enumerate() {
        if idx > 0 {
            println!();
        }
        let base = node.base();
        println!("Name      : {}", base.name);
        println!("UID       : {}", base.uid);
        println!("Created   : {}", base.creation_time.format("%Y-%m-%d %H:%M:%S UTC"));
        if let Some(p) = &base.parent_uid {
            println!("Parent UID: {}", p);
        }
        match node {
            Node::Folder(_) | Node::Album(_) => {
                println!("Type      : Folder");
            }
            Node::File(f) => {
                println!("Type      : File");
                println!("MIME      : {}", f.base.media_type);
                println!("Size      : {}", format_size(f.active_revision.size_on_cloud_storage.max(0) as u64));
                println!("Revision  : {}", f.active_revision.uid.revision_id.raw());
            }
            Node::Photo(f) => {
                println!("Type      : Photo");
                println!("MIME      : {}", f.base.media_type);
                println!("Size      : {}", format_size(f.active_revision.size_on_cloud_storage.max(0) as u64));
            }
        }
    }
    Ok(())
}
