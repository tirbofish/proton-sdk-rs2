use dialoguer::{theme::ColorfulTheme, Confirm};
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::utils::PotentialObject;

use crate::app::AppState;

/// `drop` permanently deletes trashed items matching `pattern`.
/// Requires being in /Trash, or pass --force / -f to skip confirmation.
pub async fn drop_cmd(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let force = args.iter().any(|a| a == "--force" || a == "-f");
    let pattern = args.iter().find(|a| !a.starts_with('-')).map(|s| s.as_str());

    let items = state.drive.enumerate_trash().await?;
    let targets: Vec<NodeUid> = items
        .iter()
        .filter(|item| {
            let name = node_name(item);
            match pattern {
                Some(p) => glob::Pattern::new(p).map(|pat| pat.matches(&name)).unwrap_or(false),
                None => true,
            }
        })
        .map(node_uid)
        .collect();

    if targets.is_empty() {
        println!("drop: no matching items in trash");
        return Ok(());
    }

    if !force {
        let confirmed = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!("Permanently delete {} item(s)?", targets.len()))
            .default(false)
            .interact()?;
        if !confirmed {
            println!("Aborted");
            return Ok(());
        }
    }

    let pb = crate::ui::spinner(format!("Deleting {} item(s)…", targets.len()));
    let results = match state.drive.delete_nodes_from_trash(targets).await {
        Ok(r) => { pb.finish_and_clear(); r }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    };
    let mut errors = 0usize;
    for (uid, res) in &results {
        if let Err(e) = res {
            eprintln!("drop: failed to delete {uid}: {e}");
            errors += 1;
        }
    }
    let ok_count = results.len() - errors;
    if errors == 0 {
        crate::ui::ok(format!("Permanently deleted {} item(s)", ok_count));
    }
    Ok(())
}

/// `restore` brings trashed items back to their original location.
pub async fn restore(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let pattern = args.first().map(|s| s.as_str());

    let pb = crate::ui::spinner("Loading trash…");
    let items = match state.drive.enumerate_trash().await {
        Ok(v) => { pb.finish_and_clear(); v }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    };
    let targets: Vec<NodeUid> = items
        .iter()
        .filter(|item| {
            let name = node_name(item);
            match pattern {
                Some(p) => glob::Pattern::new(p).map(|pat| pat.matches(&name)).unwrap_or(false),
                None => true,
            }
        })
        .map(node_uid)
        .collect();

    if targets.is_empty() {
        println!("restore: no matching items in trash");
        return Ok(());
    }

    let pb = crate::ui::spinner(format!("Restoring {} item(s)…", targets.len()));
    let results = match state.drive.restore_nodes(targets).await {
        Ok(r) => { pb.finish_and_clear(); r }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    };
    let mut errors = 0usize;
    for (uid, res) in &results {
        if let Err(e) = res {
            eprintln!("restore: failed to restore {uid}: {e}");
            errors += 1;
        }
    }
    let ok_count = results.len() - errors;
    if errors == 0 {
        crate::ui::ok(format!("Restored {} item(s)", ok_count));
    }

    // Invalidate cwd index so next ls shows restored items.
    if let Some(root) = state.section_root_uid() {
        state.index.unmark_indexed(&root);
    }
    Ok(())
}

fn node_name(item: &Result<Node, DegradedNode>) -> String {
    match item {
        Ok(Node::Folder(f) | Node::Album(f)) => f.base.name.clone(),
        Ok(Node::File(f) | Node::Photo(f)) => f.base.base.name.clone(),
        Err(DegradedNode::Folder(f) | DegradedNode::Album(f)) => match &f.base.name {
            PotentialObject::Node(s) => s.clone(),
            PotentialObject::Degraded(_) => "<encrypted>".to_string(),
        },
        Err(DegradedNode::File(f) | DegradedNode::Photo(f)) => match &f.base.name {
            PotentialObject::Node(s) => s.clone(),
            PotentialObject::Degraded(_) => "<encrypted>".to_string(),
        },
    }
}

fn node_uid(item: &Result<Node, DegradedNode>) -> NodeUid {
    match item {
        Ok(n) => n.uid().clone(),
        Err(d) => d.uid().clone(),
    }
}
