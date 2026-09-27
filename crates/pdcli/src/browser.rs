use std::collections::HashMap;

use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::utils::PotentialObject;
use serde::Serialize;

use crate::{daemon, flags::BrowseCommand};

#[derive(Serialize)]
struct Folder {
    uid: String,
    name: String,
    parent_uid: Option<String>,
}

#[derive(Serialize)]
struct Item {
    uid: String,
    name: String,
    kind: &'static str,
    size: Option<i64>,
    degraded: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct Listing {
    folder: Folder,
    items: Vec<Item>,
}

fn item(node: PotentialObject<Node, DegradedNode>) -> Item {
    match node {
        PotentialObject::Node(node) => {
            let (kind, size) = match &node {
                Node::Folder(_) => ("folder", None),
                Node::Album(_) => ("album", None),
                Node::File(file) => ("file", file.active_revision.claimed_size),
                Node::Photo(file) => ("photo", file.active_revision.claimed_size),
            };
            Item {
                uid: node.uid().raw(),
                name: node.base().name.clone(),
                kind,
                size,
                degraded: false,
                error: None,
            }
        }
        PotentialObject::Degraded(node) => {
            let kind = match &node {
                DegradedNode::Folder(_) => "folder",
                DegradedNode::Album(_) => "album",
                DegradedNode::File(_) => "file",
                DegradedNode::Photo(_) => "photo",
            };
            let base = match &node {
                DegradedNode::Folder(folder) | DegradedNode::Album(folder) => &folder.base,
                DegradedNode::File(file) | DegradedNode::Photo(file) => &file.base,
            };
            Item {
                uid: node.uid().raw(),
                name: match &base.name {
                    PotentialObject::Node(name) => name.clone(),
                    PotentialObject::Degraded(_) => "Unable to decrypt name".into(),
                },
                kind,
                size: None,
                degraded: true,
                error: Some(
                    base.errors
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
            }
        }
    }
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.trim().is_empty() && !name.contains(['/', '\\', '\0']) && name != "." && name != "..",
        "invalid file or folder name"
    );
    Ok(())
}

fn parse_uid(value: &str) -> anyhow::Result<NodeUid> {
    NodeUid::parse(value).map_err(anyhow::Error::msg)
}

fn require_success(
    uid: &NodeUid,
    results: &HashMap<NodeUid, Result<(), anyhow::Error>>,
) -> anyhow::Result<()> {
    results
        .get(uid)
        .ok_or_else(|| anyhow::anyhow!("no result returned for node {uid}"))?
        .as_ref()
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("{error}"))
}

pub async fn run_cli(force_offline: bool, command: BrowseCommand) -> anyhow::Result<()> {
    let session = daemon::restore_session(force_offline).await?;
    let drive = ProtonDriveClient::new(&session, None)?;
    match command {
        BrowseCommand::List { folder } => {
            let folder = match folder {
                Some(uid) => {
                    let node = drive
                        .get_node(parse_uid(&uid)?)
                        .await?
                        .result()
                        .map_err(|error| anyhow::anyhow!("{error}"))?;
                    match node {
                        Node::Folder(folder) | Node::Album(folder) => folder,
                        _ => anyhow::bail!("node is not a folder"),
                    }
                }
                None => drive.get_my_files_folder().await?,
            };
            let children = drive
                .list_children(
                    folder.base.uid.volume_id.clone(),
                    Some(folder.base.uid.link_id.clone()),
                )
                .await?;
            let listing = Listing {
                folder: Folder {
                    uid: folder.base.uid.raw(),
                    name: folder.base.name,
                    parent_uid: folder.base.parent_uid.map(|uid| uid.raw()),
                },
                items: children.into_iter().map(item).collect(),
            };
            println!("{}", serde_json::to_string(&listing)?);
        }
        BrowseCommand::Mkdir { parent, name } => {
            validate_name(&name)?;
            drive.create_folder(parse_uid(&parent)?, name, None).await?;
        }
        BrowseCommand::Rename { node, name } => {
            validate_name(&name)?;
            drive.rename_node(parse_uid(&node)?, name, None).await?;
        }
        BrowseCommand::Trash { node } => {
            let uid = parse_uid(&node)?;
            let results = drive.trash_nodes(vec![uid.clone()]).await?;
            require_success(&uid, &results)?;
        }
        BrowseCommand::Url { node } => {
            println!("{}", drive.get_node_url(parse_uid(&node)?).await?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_and_per_node_errors_are_checked() {
        assert!(validate_name("Photos 2026").is_ok());
        for name in ["", " ", ".", "..", "a/b", "a\\b"] {
            assert!(validate_name(name).is_err());
        }
        let uid = NodeUid::from_parts("volume", "link");
        let mut results = HashMap::new();
        assert!(require_success(&uid, &results).is_err());
        results.insert(uid.clone(), Err(anyhow::anyhow!("access denied")));
        assert!(
            require_success(&uid, &results)
                .unwrap_err()
                .to_string()
                .contains("access denied")
        );
    }
}
