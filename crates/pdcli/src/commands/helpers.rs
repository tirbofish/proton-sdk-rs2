use anyhow::{anyhow, Result};
use futures::stream::StreamExt;
use glob::Pattern;
use indicatif::{ProgressBar, ProgressStyle};
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::utils::PotentialObject;
use std::sync::Arc;

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
            let s = $state.lock().await;
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

/// Find a child by name in the current directory; returns its uid and the node.
pub async fn find_child_by_name(
    client: &ProtonDriveClient,
    folder_uid: NodeUid,
    name: &str,
) -> Result<Node> {
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

pub fn get_available_name(existing_nodes: &[Node], base_name: &str) -> Result<String> {
    if !existing_nodes.iter().any(|n| n.base().name == base_name) {
        return Ok(base_name.to_string());
    }

    let (name, ext) = if let Some(dot_pos) = base_name.rfind('.') {
        (&base_name[..dot_pos], &base_name[dot_pos..])
    } else {
        (base_name, "")
    };

    for i in 1..1000 {
        let candidate = format!("{}({}){}", name, i, ext);
        if !existing_nodes.iter().any(|n| n.base().name == candidate) {
            return Ok(candidate);
        }
    }

    Err(anyhow!("Could not find available name"))
}

pub fn progress_bar_for(label: &str) -> Arc<ProgressBar> {
    let pb = ProgressBar::new(0);
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

pub fn expand_local_path(path: &str) -> std::path::PathBuf {
    if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home);
        }
    }

    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
    }

    std::path::PathBuf::from(path)
}

pub async fn resolve_folder_path(
    client: &ProtonDriveClient,
    current_uid: NodeUid,
    current_path: Vec<String>,
    root_uid: NodeUid,
    path: &str,
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

        let child = find_child_by_name(client, uid.clone(), seg).await?;
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
