//! Listing, restoring and permanently deleting items in the Drive trash.

use std::collections::HashMap;

use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::utils::PotentialObject;
use serde::Serialize;

use crate::daemon;
use crate::flags::TrashCommand;

#[derive(Serialize)]
struct TrashItem {
    uid: String,
    name: String,
    kind: &'static str,
    size: Option<i64>,
    trashed_at: Option<String>,
}

fn trash_item(node: Result<Node, DegradedNode>) -> TrashItem {
    match node {
        Ok(node) => {
            let (kind, size) = match &node {
                Node::Folder(_) => ("folder", None),
                Node::Album(_) => ("album", None),
                Node::File(file) => ("file", file.active_revision.claimed_size),
                Node::Photo(file) => ("photo", file.active_revision.claimed_size),
            };
            TrashItem {
                uid: node.uid().raw(),
                name: node.base().name.clone(),
                kind,
                size,
                trashed_at: node.base().trash_time.map(|time| time.to_rfc3339()),
            }
        }
        Err(node) => {
            let (kind, base) = match &node {
                DegradedNode::Folder(folder) => ("folder", &folder.base),
                DegradedNode::Album(folder) => ("album", &folder.base),
                DegradedNode::File(file) => ("file", &file.base),
                DegradedNode::Photo(file) => ("photo", &file.base),
            };
            TrashItem {
                uid: node.uid().raw(),
                name: match &base.name {
                    PotentialObject::Node(name) => name.clone(),
                    PotentialObject::Degraded(_) => "Unable to decrypt name".into(),
                },
                kind,
                size: None,
                trashed_at: base.trash_time.map(|time| time.to_rfc3339()),
            }
        }
    }
}

pub async fn run_cli(force_offline: bool, command: TrashCommand) -> anyhow::Result<()> {
    let session = daemon::restore_session(force_offline).await?;
    let drive = ProtonDriveClient::new(&session, None)?;
    match command {
        TrashCommand::List { json } => {
            let mut items: Vec<TrashItem> = drive
                .enumerate_trash()
                .await?
                .into_iter()
                .map(trash_item)
                .collect();
            items.sort_by(|a, b| b.trashed_at.cmp(&a.trashed_at));
            if json {
                println!("{}", serde_json::to_string(&items)?);
            } else if items.is_empty() {
                println!("trash is empty");
            } else {
                for item in items {
                    println!(
                        "{}\t{}\t{}\t{}",
                        item.trashed_at.as_deref().unwrap_or("-"),
                        item.kind,
                        item.name,
                        item.uid
                    );
                }
            }
        }
        TrashCommand::Restore { uids } => {
            let uids = parse_uids(&uids)?;
            report("restored", &uids, drive.restore_nodes(uids.clone()).await?)?;
        }
        TrashCommand::Delete { uids } => {
            let uids = parse_uids(&uids)?;
            report(
                "permanently deleted",
                &uids,
                drive.delete_nodes_from_trash(uids.clone()).await?,
            )?;
        }
        TrashCommand::Empty => {
            drive.empty_trash().await?;
            println!("trash emptied");
        }
    }
    Ok(())
}

fn parse_uids(values: &[String]) -> anyhow::Result<Vec<NodeUid>> {
    values
        .iter()
        .map(|value| NodeUid::parse(value).map_err(anyhow::Error::msg))
        .collect()
}

/// Prints a summary, and fails if any item failed, naming each failure.
fn report(
    done: &str,
    uids: &[NodeUid],
    results: HashMap<NodeUid, Result<(), anyhow::Error>>,
) -> anyhow::Result<()> {
    let mut failed = 0;
    for uid in uids {
        match results.get(uid) {
            Some(Ok(())) => {}
            Some(Err(error)) => {
                eprintln!("{uid}: {error}");
                failed += 1;
            }
            None => {
                eprintln!("{uid}: no result returned");
                failed += 1;
            }
        }
    }
    let count = uids.len() - failed;
    println!("{done} {count} item{}", if count == 1 { "" } else { "s" });
    anyhow::ensure!(failed == 0, "{failed} item(s) failed");
    Ok(())
}
