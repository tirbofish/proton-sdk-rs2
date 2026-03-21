use anyhow::{anyhow, Result};
use futures::stream::StreamExt;
use glob::Pattern;
use indicatif::{ProgressBar, ProgressStyle};
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::utils::PotentialObject;
use proton_drive_sdk::volume::VolumeId;
use std::sync::Arc;
use crate::rusqlite_cache::RusqliteCache;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    Top,
    MyFiles,
    Trash,
}

pub fn area_from_path(path: &[String]) -> Area {
    match path.first().map(String::as_str) {
        Some("MyFiles") => Area::MyFiles,
        Some("Trash") => Area::Trash,
        _ => Area::Top,
    }
}

/// Clone the client + snapshot state we need, all in one lock
#[macro_export]
macro_rules! auth_snapshot {
    ($state:expr => $client:ident, $uid:ident) => {
        let ($client, $uid) = {
            let s = $state.lock();
            let c = s
                .get_client()
                .ok_or_else(|| anyhow::anyhow!("Not authenticated. Use 'login' first."))?
                .clone();
            let u = s
                .get_current_node_uid()
                .ok_or_else(|| anyhow::anyhow!("No current directory"))?
                .clone();
            (c, u)
        };
    };
}

/// Enumerate children of `folder_uid` and collect all decrypted `Node`s
pub async fn list_children(
    client: &ProtonDriveClient,
    folder_uid: NodeUid,
) -> Result<Vec<Node>> {
    let mut stream = client.enumerate_folder_children(folder_uid).await?;
    let mut nodes = Vec::new();
    while let Some(item) = stream.next().await {
        if let Ok(PotentialObject::Node(node)) = item {
            nodes.push(node);
        }
    }
    Ok(nodes)
}

/// Find a child by name. Checks the local SQLite cache first; fetches from the
/// network (enumerate + decrypt all children) only on a cache miss.
pub async fn find_child_by_name(
    client: &ProtonDriveClient,
    folder_uid: NodeUid,
    name: &str,
    cache: Option<&RusqliteCache>,
) -> Result<Node> {
    // Fast path: SQLite lookup — O(1) indexed query, no network.
    if let Some(c) = cache {
        if let Ok(Some(cached)) = c.get_child_by_name(&folder_uid.volume_id, Some(&folder_uid.link_id), name) {
            let uid = NodeUid::new(
                VolumeId::new(cached.volume_id),
                LinkId::new(cached.link_id),
            );
            // Fetch just this one node from the network to get the full decrypted Node.
            if let Ok(proton_drive_sdk::utils::PotentialObject::Node(node)) = client.get_node(uid).await {
                return Ok(node);
            }
            // Cache hit but network fetch failed — fall through to full enumeration.
        }
    }
    // Slow path: enumerate and decrypt all children, then scan by name.
    let children = list_children(client, folder_uid).await?;
    children
        .into_iter()
        .find(|n| n.base().name == name)
        .ok_or_else(|| anyhow!("'{}' not found in current directory", name))
}

pub fn has_wildcards(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

pub fn selector_matches(selector: &str, name: &str) -> Result<bool> {
    if has_wildcards(selector) {
        Ok(Pattern::new(selector)?.matches(name))
    } else {
        Ok(selector == name)
    }
}

pub fn degraded_name(node: &DegradedNode) -> String {
    match node {
        DegradedNode::Folder(n) | DegradedNode::Album(n) => n
            .base
            .name
            .clone()
            .result()
            .unwrap_or_else(|_| "<unknown>".to_string()),
        DegradedNode::File(n) | DegradedNode::Photo(n) => n
            .base
            .name
            .clone()
            .result()
            .unwrap_or_else(|_| "<unknown>".to_string()),
    }
}

pub fn split_parent_and_leaf(input: &str) -> (&str, &str) {
    if let Some((parent, leaf)) = input.rsplit_once('/') {
        let parent = if parent.is_empty() { "/" } else { parent };
        (parent, leaf)
    } else {
        (".", input)
    }
}

pub fn new_spinner(msg: impl Into<String>) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("  {spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(msg.into());
    pb.enable_steady_tick(std::time::Duration::from_millis(80));
    pb
}

pub fn progress_bar_for(label: &str, total: u64) -> Arc<ProgressBar> {
    let pb = ProgressBar::new(total);
    let style = ProgressStyle::with_template(
        "{spinner:.cyan} {msg:20.20} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({percent}%)",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("##-");
    pb.set_style(style);
    pb.set_message(label.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(80));
    Arc::new(pb)
}

pub fn progress_callback(pb: Arc<ProgressBar>) -> Box<dyn Fn(i64, i64) + Send + Sync> {
    Box::new(move |done, total| {
        if total > 0 {
            pb.set_length(total as u64);
        }
        if done >= 0 {
            pb.set_position(done as u64);
        }
    })
}


pub async fn resolve_folder_path(
    client: &ProtonDriveClient,
    current_uid: NodeUid,
    current_path: Vec<String>,
    root_uid: NodeUid,
    path: &str,
    cache: Option<&RusqliteCache>,
) -> Result<(NodeUid, Vec<String>)> {
    if path.is_empty() || path == "." {
        return Ok((current_uid, current_path));
    }

    let mut uid = if path.starts_with('/') || path.starts_with("MyFiles/") || path == "MyFiles" {
        root_uid
    } else {
        current_uid
    };

    let mut path_vec = if path.starts_with('/') || path.starts_with("MyFiles") {
        vec!["MyFiles".to_string()]
    } else {
        current_path
    };

    let normalized = path.trim_start_matches('/').trim_start_matches("MyFiles/");
    if normalized.is_empty() {
        return Ok((uid, path_vec));
    }

    for seg in normalized.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            let n = client.get_node(uid.clone()).await?;
            let parent_uid = match n {
                PotentialObject::Node(node) => node.base().parent_uid.clone(),
                PotentialObject::Degraded(node) => node.parent_uid().cloned(),
            };
            if let Some(parent) = parent_uid {
                uid = parent;
                if path_vec.len() > 1 {
                    path_vec.pop();
                }
            }
            continue;
        }

        let child = find_child_by_name(client, uid.clone(), seg, cache).await?;
        if !matches!(child, Node::Folder(_) | Node::Album(_)) {
            return Err(anyhow!("'{}' is not a directory", seg));
        }
        uid = child.uid().clone();
        path_vec.push(seg.to_string());
    }

    Ok((uid, path_vec))
}

/// Format bytes as human-readable size
pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{} B", size as u64)
    } else {
        format!("{:.2} {}", size, UNITS[unit_idx])
    }
}
