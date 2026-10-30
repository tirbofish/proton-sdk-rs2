//! Public share links: inspecting and downloading them, and the bookmarks that
//! keep links others sent you.

use std::path::{Path, PathBuf};

use anyhow::Context;
use futures::StreamExt;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::sharing::PublicLinkClient;
use proton_drive_sdk::utils::PotentialObject;
use serde::Serialize;
use tokio::io::AsyncReadExt;

use crate::daemon;
use crate::flags::PublicCommand;

#[derive(Serialize)]
struct Bookmark {
    uid: String,
    url: String,
    name: Option<String>,
    created_at: Option<String>,
}

pub async fn run_cli(force_offline: bool, command: PublicCommand) -> anyhow::Result<()> {
    let session = daemon::restore_session(force_offline).await?;
    let drive = ProtonDriveClient::new(&session, None)?;
    match command {
        PublicCommand::Info {
            url,
            password_stdin,
        } => {
            let link = open_link(&drive, &url, password_stdin).await?;
            let root = decrypted(link.get_root_node().await?)?;
            let (kind, size) = match &root {
                Node::Folder(_) => ("folder", None),
                Node::Album(_) => ("album", None),
                Node::File(file) | Node::Photo(file) => ("file", file.active_revision.claimed_size),
            };
            println!("name\t{}", root.base().name);
            println!("kind\t{kind}");
            if let Some(size) = size {
                println!("size\t{size}");
            }
        }
        PublicCommand::Download {
            url,
            destination,
            password_stdin,
        } => {
            let link = open_link(&drive, &url, password_stdin).await?;
            let root = decrypted(link.get_root_node().await?)?;
            let name = safe_name(&root.base().name)?;
            // An existing directory receives the item under its own name.
            let target = if destination.is_dir() {
                destination.join(name)
            } else {
                destination
            };
            let count = download_node(&link, root, &target).await?;
            println!("{}", serde_json::json!({ "path": target, "files": count }));
        }
        PublicCommand::Bookmarks { json } => {
            let bookmarks: Vec<Bookmark> = drive
                .iterate_bookmarks()
                .await?
                .into_iter()
                .map(|bookmark| Bookmark {
                    uid: bookmark.uid,
                    url: bookmark.url,
                    name: bookmark.node_name,
                    created_at: bookmark.creation_time.map(|time| time.to_rfc3339()),
                })
                .collect();
            if json {
                println!("{}", serde_json::to_string(&bookmarks)?);
            } else if bookmarks.is_empty() {
                println!("no bookmarks");
            } else {
                for bookmark in bookmarks {
                    println!(
                        "{}\t{}\t{}",
                        bookmark.name.as_deref().unwrap_or("-"),
                        bookmark.url,
                        bookmark.uid
                    );
                }
            }
        }
        PublicCommand::Bookmark { url } => {
            drive.create_bookmark(&url).await?;
            println!("bookmarked");
        }
        PublicCommand::Unbookmark { bookmark } => {
            drive.remove_bookmark(&bookmark).await?;
            println!("bookmark removed");
        }
    }
    Ok(())
}

/// Signs in to a public link. A link with a custom password needs it appended
/// to the generated one in the URL fragment, read from stdin so it never
/// appears in the process list.
async fn open_link(
    drive: &ProtonDriveClient,
    url: &str,
    password_stdin: bool,
) -> anyhow::Result<PublicLinkClient> {
    let info = drive.get_public_link_info(url).await?;
    let url = if info.is_custom_password_protected {
        anyhow::ensure!(
            password_stdin,
            "this link has a custom password; pass it with --password-stdin"
        );
        let mut password = String::new();
        tokio::io::stdin().read_to_string(&mut password).await?;
        let password = password.trim_end_matches(['\r', '\n']);
        anyhow::ensure!(!password.is_empty(), "no password on standard input");
        with_custom_password(url, password)?
    } else {
        url.to_string()
    };
    drive.authenticate_public_link(&url).await
}

fn with_custom_password(url: &str, custom: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        url.contains('#'),
        "the link is missing its password fragment"
    );
    Ok(format!("{url}{custom}"))
}

fn decrypted(node: PotentialObject<Node, DegradedNode>) -> anyhow::Result<Node> {
    match node {
        PotentialObject::Node(node) => Ok(node),
        PotentialObject::Degraded(_) => anyhow::bail!("the shared item could not be decrypted"),
    }
}

/// A name from someone else's share, refused if it could leave the
/// destination folder.
fn safe_name(name: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0']),
        "refusing unsafe file name {name:?}"
    );
    Ok(name)
}

/// Downloads a file, or a folder and everything below it, returning how
/// many files were written.
async fn download_node(
    link: &PublicLinkClient,
    root: Node,
    target: &Path,
) -> anyhow::Result<usize> {
    let mut pending: Vec<(Node, PathBuf)> = vec![(root, target.to_path_buf())];
    let mut count = 0;
    while let Some((node, path)) = pending.pop() {
        match node {
            Node::File(file) | Node::Photo(file) => {
                crate::browser::download_to_path(link.drive(), file.active_revision.uid, &path)
                    .await?;
                eprintln!("{}", path.display());
                count += 1;
            }
            Node::Folder(_) | Node::Album(_) => {
                std::fs::create_dir_all(&path)
                    .with_context(|| format!("create {}", path.display()))?;
                let uid: NodeUid = node.uid().clone();
                let mut children = Box::pin(link.enumerate_folder_children(uid).await?);
                while let Some(child) = children.next().await {
                    let child = decrypted(child?)?;
                    let child_path = path.join(safe_name(&child.base().name)?);
                    pending.push((child, child_path));
                }
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_names_cannot_escape_the_destination() {
        assert!(safe_name("report.pdf").is_ok());
        for name in ["", ".", "..", "../x", "a/b", "a\\b", "nul\0"] {
            assert!(safe_name(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn custom_password_follows_the_generated_one() {
        assert_eq!(
            with_custom_password("https://drive.proton.me/urls/TOKEN#generated", "secret").unwrap(),
            "https://drive.proton.me/urls/TOKEN#generatedsecret"
        );
    }
}
