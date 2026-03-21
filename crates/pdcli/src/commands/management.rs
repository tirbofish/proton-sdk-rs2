use crate::state::ReplState;
use anyhow::{anyhow, Result};
use dialoguer::Confirm;
use glob::Pattern;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::utils::PotentialObject;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use parking_lot::Mutex;

use super::helpers::{
    area_from_path, degraded_name, find_child_by_name, format_size,
    has_wildcards, list_children, new_spinner, resolve_folder_path, selector_matches,
    split_parent_and_leaf, Area,
};

pub async fn mkdir_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: mkdir <folder_name>"));
    }
    let name = args[0].to_string();
    
    let (client, parent_uid, cache) = {
        let s = state.lock();
        let c = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let u = s
            .get_current_node_uid()
            .ok_or_else(|| anyhow!("No current directory"))?
            .clone();
        let cache = s.get_cache();
        (c, u, cache)
    };

    // Frontend-first: insert a provisional folder in the cache immediately.
    let provisional_id = format!("pending:{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0));
    if let Some(ref c) = cache {
        let _ = c.insert_provisional_node(
            &parent_uid.volume_id,
            &provisional_id,
            Some(&parent_uid.link_id),
            &name,
            "Folder",
            None,
        );
    }

    let sp = new_spinner(format!("Creating '{}'...", name));
    let result = client.create_folder(parent_uid.clone(), name.clone(), None).await;
    sp.finish_and_clear();

    match result {
        Ok(new_folder) => {
            // Replace provisional with the real node.
            if let Some(ref c) = cache {
                let _ = c.delete_node(&parent_uid.volume_id, &proton_drive_sdk::links::LinkId::new(provisional_id));
                let _ = c.upsert_node(&Node::Folder(new_folder.clone()), false);
            }
            println!("Created: {}", name);
            Ok(())
        }
        Err(e) => {
            // Revert provisional entry.
            if let Some(ref c) = cache {
                let _ = c.delete_node(&parent_uid.volume_id, &proton_drive_sdk::links::LinkId::new(provisional_id));
            }
            Err(e)
        }
    }
}

pub async fn move_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.len() < 2 {
        return Err(anyhow!("Usage: mv <src> <dst>"));
    }

    let mut arg_start = 0;
    for (i, arg) in args.iter().enumerate() {
        match *arg {
            "-f" | "--force" => {}
            _ => { arg_start = i; break; }
        }
    }

    if args.len() - arg_start < 2 {
        return Err(anyhow!("Usage: mv <src> <dst>"));
    }

    let src = args[arg_start];
    let dst = args[arg_start + 1];

    let (client, current_uid, root_uid, current_path, cache) = {
        let s = state.lock();
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
        let cache = s.get_cache();
        (client, uid, root_uid, s.get_current_path().to_vec(), cache)
    };

    // Show the spinner immediately — before any network work — so there is no
    // visible gap between entering the command and seeing feedback.
    let sp = new_spinner(format!("Moving '{}' → '{}'...", src, dst));

    if has_wildcards(src) {
        let pattern = Pattern::new(src)?;
        let children = list_children(&client, current_uid.clone()).await?;
        let matches: Vec<Node> = children
            .into_iter()
            .filter(|n| pattern.matches(&n.base().name))
            .collect();
        if matches.is_empty() {
            sp.finish_and_clear();
            return Err(anyhow!("No files matched pattern '{}'", src));
        }

        let (dst_uid, _) = resolve_folder_path(
            &client,
            current_uid,
            current_path,
            root_uid,
            dst,
            cache.as_deref(),
        )
        .await?;

        let count = matches.len();
        sp.set_message(format!("Moving {} item(s) to '{}'...", count, dst));
        let uids = matches.iter().map(|n| n.uid().clone()).collect::<Vec<_>>();
        client.move_nodes(uids, dst_uid).await?;
        sp.finish_and_clear();
        println!("Moved {} item(s) to '{}'", count, dst);
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
            cache.as_deref(),
        )
        .await?
        .0
    };

    let node = find_child_by_name(&client, src_folder_uid.clone(), src_leaf, cache.as_deref()).await?;
    let node_uid = node.uid().clone();

    let (target_folder_uid, final_name) = if dst.contains('/') {
        let (folder_part, maybe_name) = match dst.rsplit_once('/') {
            Some((f, n)) => (f, n),
            None => ("", dst),
        };
        let target_folder_path = if folder_part.is_empty() { "." } else { folder_part };
        let (t_uid, _) = resolve_folder_path(
            &client,
            current_uid.clone(),
            current_path.clone(),
            root_uid.clone(),
            target_folder_path,
            cache.as_deref(),
        )
        .await?;
        (t_uid, if maybe_name.is_empty() { node.base().name.clone() } else { maybe_name.to_string() })
    } else {
        // Check whether dst names an existing folder: consult the cache first to
        // avoid an extra network round-trip in the common rename-in-place case.
        let dst_is_folder = if let Some(ref c) = cache {
            match c.get_child_by_name(&current_uid.volume_id, Some(&current_uid.link_id), dst) {
                Ok(Some(cached)) => cached.node_type == "Folder" || cached.node_type == "Album",
                // Cache miss — fall back to network only if needed
                _ => {
                    matches!(
                        find_child_by_name(&client, current_uid.clone(), dst, cache.as_deref()).await,
                        Ok(Node::Folder(_) | Node::Album(_))
                    )
                }
            }
        } else {
            matches!(
                find_child_by_name(&client, current_uid.clone(), dst, cache.as_deref()).await,
                Ok(Node::Folder(_) | Node::Album(_))
            )
        };

        if dst_is_folder {
            // dst is a directory — move src into it keeping the original name.
            let (t_uid, _) = resolve_folder_path(
                &client,
                current_uid.clone(),
                current_path.clone(),
                root_uid.clone(),
                dst,
                cache.as_deref(),
            ).await?;
            (t_uid, node.base().name.clone())
        } else {
            (current_uid.clone(), dst.to_string())
        }
    };

    let needs_move = target_folder_uid != *node.base().parent_uid.as_ref().unwrap_or(&target_folder_uid);
    let needs_rename = final_name != node.base().name;

    // Frontend-first: apply name/parent change to cache optimistically.
    if let Some(ref c) = cache {
        let new_parent = if needs_move { Some(&target_folder_uid.link_id) } else { None };
        let new_name = if needs_rename { Some(final_name.as_str()) } else { None };
        let _ = c.rename_cached_node(&node_uid.volume_id, &node_uid.link_id, new_name, new_parent);
    }

    let move_result = if needs_move {
        Some(client.move_nodes(vec![node_uid.clone()], target_folder_uid.clone()).await)
    } else { None };

    if let Some(Err(e)) = move_result {
        // Revert: restore original parent/name in cache.
        if let Some(ref c) = cache {
            let orig_parent = node.base().parent_uid.as_ref().map(|p| &p.link_id);
            let _ = c.rename_cached_node(&node_uid.volume_id, &node_uid.link_id, Some(&node.base().name), orig_parent);
        }
        return Err(e);
    }

    let rename_result = if needs_rename {
        Some(client.rename_node(node_uid.clone(), final_name.clone(), None).await)
    } else { None };

    if let Some(Err(e)) = rename_result {
        // Revert cache name.
        if let Some(ref c) = cache {
            let orig_parent = node.base().parent_uid.as_ref().map(|p| &p.link_id);
            let _ = c.rename_cached_node(&node_uid.volume_id, &node_uid.link_id, Some(&node.base().name), orig_parent);
        }
        return Err(e);
    }

    // Fetch the authoritative node from the server to replace the optimistic entry.
    if let Some(ref c) = cache {
        if let Ok(PotentialObject::Node(fresh)) = client.get_node(node_uid.clone()).await {
            let _ = c.upsert_node(&fresh, false);
        }
    }

    sp.finish_and_clear();
    println!("Moved '{}' → '{}'", src, dst);
    Ok(())
}

pub async fn remove_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: rm <path|pattern> ..."));
    }

    let mut force = false;
    let mut targets: Vec<&str> = Vec::new();
    for arg in args {
        match *arg {
            "-r" => {} // Proton Drive trashes recursively by default
            "-f" => force = true,
            "-rf" | "-fr" => {
                force = true;
            }
            _ => targets.push(arg),
        }
    }

    if targets.is_empty() {
        return Err(anyhow!("Usage: rm <path|pattern> ..."));
    }

    let (client, current_uid, current_path, root_uid, cache) = {
        let s = state.lock();
        let client = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let root_uid = s
            .get_root_node_uid()
            .ok_or_else(|| anyhow!("Root unknown; please re-login"))?
            .clone();
        let cache = s.get_cache();
        (
            client,
            s.get_current_node_uid().cloned(),
            s.get_current_path().to_vec(),
            root_uid,
            cache,
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
                cache.as_deref(),
            )
            .await?
            .0
        };

        if has_wildcards(leaf_selector) {
            // Wildcard: use cache list for quick in-memory match.
            let children = if let Some(ref c) = cache {
                let cached = c.list_children(&target_uid.volume_id, Some(&target_uid.link_id))?;
                if !cached.is_empty() {
                    // Turn CachedNodes directly into UIDs — no network needed.
                    cached.into_iter()
                        .filter(|n| selector_matches(leaf_selector, &n.name).unwrap_or(false))
                        .map(|n| NodeUid::new(target_uid.volume_id.clone(), proton_drive_sdk::links::LinkId::new(n.link_id)))
                        .collect::<Vec<_>>()
                } else {
                    list_children(&client, target_uid.clone()).await?
                        .into_iter()
                        .filter(|n| selector_matches(leaf_selector, &n.base().name).unwrap_or(false))
                        .map(|n| n.uid().clone())
                        .collect()
                }
            } else {
                list_children(&client, target_uid.clone()).await?
                    .into_iter()
                    .filter(|n| selector_matches(leaf_selector, &n.base().name).unwrap_or(false))
                    .map(|n| n.uid().clone())
                    .collect()
            };
            to_trash.extend(children);
            continue;
        }

        // Exact name: find_child_by_name already does cache-first.
        match find_child_by_name(&client, target_uid, leaf_selector, cache.as_deref()).await {
            Ok(node) => to_trash.push(node.uid().clone()),
            Err(_) if force => {}
            Err(_) => {
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

    // Frontend-first: mark nodes as trashed in cache immediately so ls reflects
    // the change without waiting for the network round-trip.
    if let Some(ref c) = cache {
        for uid in &to_trash {
            let _ = c.mark_node_trashed(&uid.volume_id, &uid.link_id);
        }
    }

    let sp = new_spinner(format!("Trashing {} item(s)...", to_trash.len()));
    let result = client.trash_nodes(to_trash.clone()).await;
    sp.finish_and_clear();

    let result = match result {
        Ok(r) => r,
        Err(e) => {
            // Revert optimistic cache update on full failure.
            if let Some(ref c) = cache {
                for uid in &to_trash {
                    let _ = c.mark_node_untrashed(&uid.volume_id, &uid.link_id);
                }
            }
            return Err(e);
        }
    };

    let mut failed_uids = Vec::new();
    for uid in &to_trash {
        if let Some(Err(err)) = result.get(uid) {
            failed_uids.push(uid.clone());
            match client.get_node(uid.clone()).await {
                Ok(PotentialObject::Node(node)) => {
                    eprintln!("Failed to trash '{}'(uid={}): {}", node.base().name, uid, err);
                }
                _ => eprintln!("Failed to trash uid={}: {}", uid, err),
            }
        }
    }

    // Revert only the nodes that failed.
    if !failed_uids.is_empty() {
        if let Some(ref c) = cache {
            for uid in &failed_uids {
                let _ = c.mark_node_untrashed(&uid.volume_id, &uid.link_id);
            }
        }
        return Err(anyhow!("{} item(s) failed to trash", failed_uids.len()));
    }

    println!("Trashed {} item(s)", to_trash.len());
    Ok(())
}

pub async fn drop_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: drop <name|pattern> ..."));
    }

    let (client, area) = {
        let s = state.lock();
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

    let sp = new_spinner("Loading Trash...");
    let items = client.enumerate_trash().await?;
    sp.finish_and_clear();
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

    // Frontend-first: remove from cache immediately so ls is up to date.
    let cache = state.lock().get_cache();
    if let Some(ref c) = cache {
        for uid in &to_delete {
            let _ = c.delete_node(&uid.volume_id, &uid.link_id);
        }
    }

    let sp = new_spinner(format!("Permanently deleting {} item(s)...", to_delete.len()));
    let result = client.delete_nodes_from_trash(to_delete.clone()).await;
    sp.finish_and_clear();

    let result = match result {
        Ok(r) => r,
        Err(e) => {
            // Revert: re-mark them as trashed so they appear in trash again.
            if let Some(ref c) = cache {
                for uid in &to_delete {
                    if let Ok(Some(cn)) = c.get_node_by_uid(&uid.volume_id, &uid.link_id) {
                        // Already re-inserted if re-upsert is possible; otherwise just re-mark.
                        let _ = c.mark_node_trashed(&uid.volume_id, &uid.link_id);
                        let _ = cn; // suppress warning
                    }
                }
            }
            return Err(e);
        }
    };

    let mut failed = 0usize;
    for uid in &to_delete {
        if let Some(Err(err)) = result.get(uid) {
            failed += 1;
            let name = names_by_uid.get(uid).map(String::as_str).unwrap_or("<unknown>");
            eprintln!("Failed to drop '{}'(uid={}): {}", name, uid, err);
        }
    }
    if failed > 0 {
        return Err(anyhow!("{} item(s) failed to drop", failed));
    }

    println!("Dropped {} item(s)", to_delete.len());
    Ok(())
}

/// `restore <name|pattern>` — restore nodes from Trash back to their original location
pub async fn restore_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: restore <name|pattern> ..."));
    }

    let (client, area) = {
        let s = state.lock();
        let client = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let area = area_from_path(s.get_current_path());
        (client, area)
    };

    if area != Area::Trash {
        return Err(anyhow!("restore is only available in Trash. Navigate with: cd /Trash"));
    }

    let sp = new_spinner("Loading Trash...");
    let items = client.enumerate_trash().await?;
    sp.finish_and_clear();
    let mut to_restore = Vec::new();
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
                        to_restore.push(uid.clone());
                        names_by_uid.entry(uid).or_insert_with(|| node.base().name.clone());
                    }
                }
                Err(d) => {
                    let name = degraded_name(d);
                    if selector_matches(leaf_selector, &name)? {
                        let uid = d.uid().clone();
                        to_restore.push(uid.clone());
                        names_by_uid.entry(uid).or_insert(name);
                    }
                }
            }
        }
    }

    let mut seen = HashSet::new();
    to_restore.retain(|uid| seen.insert(uid.clone()));
    if to_restore.is_empty() {
        return Err(anyhow!("Nothing matched"));
    }

    // Frontend-first: mark nodes as untrashed so ls /Trash no longer shows them.
    let cache = state.lock().get_cache();
    if let Some(ref c) = cache {
        for uid in &to_restore {
            let _ = c.mark_node_untrashed(&uid.volume_id, &uid.link_id);
        }
    }

    let sp = new_spinner(format!("Restoring {} item(s)...", to_restore.len()));
    let result = client.restore_nodes(to_restore.clone()).await;
    sp.finish_and_clear();

    let result = match result {
        Ok(r) => r,
        Err(e) => {
            // Revert optimistic update.
            if let Some(ref c) = cache {
                for uid in &to_restore {
                    let _ = c.mark_node_trashed(&uid.volume_id, &uid.link_id);
                }
            }
            return Err(e);
        }
    };

    let mut failed_uids = Vec::new();
    for uid in &to_restore {
        if let Some(Err(err)) = result.get(uid) {
            failed_uids.push(uid.clone());
            let name = names_by_uid.get(uid).map(String::as_str).unwrap_or("<unknown>");
            eprintln!("Failed to restore '{}' (uid={}): {}", name, uid, err);
        }
    }

    if !failed_uids.is_empty() {
        // Revert only the ones that failed.
        if let Some(ref c) = cache {
            for uid in &failed_uids {
                let _ = c.mark_node_trashed(&uid.volume_id, &uid.link_id);
            }
        }
        return Err(anyhow!("{} item(s) failed to restore", failed_uids.len()));
    }

    println!("Restored {} item(s)", to_restore.len());
    Ok(())
}

/// `stat <name>` — display metadata for a node in the current directory
pub async fn stat_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        return Err(anyhow!("Usage: stat <name|pattern> ..."));
    }
    
    let (client, current_uid, current_path, root_uid, cache) = {
        let s = state.lock();
        let c = s
            .get_client()
            .ok_or_else(|| anyhow!("Not authenticated. Use 'login' first."))?
            .clone();
        let u = s
            .get_current_node_uid()
            .ok_or_else(|| anyhow!("No current directory"))?
            .clone();
        let p = s.get_current_path().to_vec();
        let r = s.get_root_node_uid().ok_or_else(|| anyhow!("Root unknown"))?.clone();
        let cache = s.get_cache();
        (c, u, p, r, cache)
    };

    let sp = new_spinner("Loading...");
    let children = list_children(&client, current_uid.clone()).await?;
    sp.finish_and_clear();
    let mut matched: Vec<Node> = Vec::new();
    for selector in args {
        let (parent_selector, leaf_selector) = split_parent_and_leaf(selector);
        let target_uid = if parent_selector == "." {
            current_uid.clone()
        } else {
            let (uid, _) = resolve_folder_path(
                &client,
                current_uid.clone(),
                current_path.clone(),
                root_uid.clone(),
                parent_selector,
                cache.as_deref(),
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

pub async fn cp_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.len() < 2 {
        return Err(anyhow!("Usage: cp <src_name|pattern> <dst_name>"));
    }

    let src = args[0];
    let dst = args[1];

    let (client, current_uid, root_uid, current_path, cache) = {
        let s = state.lock();
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
        let cache = s.get_cache();
        (client, uid, root_uid, s.get_current_path().to_vec(), cache)
    };

    // Resolve the source node
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
            cache.as_deref(),
        )
        .await?
        .0
    };

    let src_node = find_child_by_name(&client, src_folder_uid, src_leaf, cache.as_deref()).await?;
    let src_uid = src_node.uid().clone();
    let src_name = src_node.base().name.clone();

    // Determine the copy name
    let new_name = if dst.is_empty() || dst == "." {
        let original_name = &src_name;
        if let Some(dot_pos) = original_name.rfind('.') {
            let (name, ext) = original_name.split_at(dot_pos);
            format!("{} (copy){}", name, ext)
        } else {
            format!("{} (copy)", original_name)
        }
    } else {
        dst.to_string()
    };

    let sp = new_spinner(format!("Copying '{}' → '{}'...", src_name, new_name));

    // Server-side copy: re-encrypts keys and calls the copy endpoint.
    // No file data is downloaded or uploaded.
    let new_link_id = client
        .copy_node(src_uid.clone(), current_uid.clone(), Some(new_name.clone()))
        .await;

    sp.finish_and_clear();

    let new_link_id = new_link_id?;

    // Fetch the new node and insert it into the cache
    if let Some(ref c) = cache {
        let new_uid = proton_drive_sdk::node::NodeUid::new(src_uid.volume_id.clone(), new_link_id);
        if let Ok(proton_drive_sdk::utils::PotentialObject::Node(fresh)) = client.get_node(new_uid).await {
            let _ = c.upsert_node(&fresh, false);
        }
    }

    println!("Copied '{}' → '{}'", src_name, new_name);
    Ok(())
}
