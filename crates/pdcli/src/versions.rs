//! A file's earlier versions: listing, restoring, downloading and deleting them.

use anyhow::Context;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::NodeUid;
use proton_drive_sdk::node::revision::{RevisionState, RevisionUid};
use serde::Serialize;

use crate::daemon;
use crate::flags::VersionsCommand;

#[derive(Serialize)]
struct Version {
    uid: String,
    current: bool,
    created_at: String,
    modified_at: Option<String>,
    size: Option<i64>,
}

fn parse_revision(value: &str) -> anyhow::Result<RevisionUid> {
    RevisionUid::try_parse(value).with_context(|| format!("not a version ID: {value}"))
}

pub async fn run_cli(force_offline: bool, command: VersionsCommand) -> anyhow::Result<()> {
    let session = daemon::restore_session(force_offline).await?;
    let drive = ProtonDriveClient::new(&session, None)?;
    match command {
        VersionsCommand::List { node, json } => {
            let node = NodeUid::parse(&node).map_err(anyhow::Error::msg)?;
            let mut versions: Vec<Version> = drive
                .iterate_revisions(node)
                .await?
                .into_iter()
                .map(|revision| Version {
                    uid: revision.uid.to_string(),
                    current: matches!(revision.state, RevisionState::Active),
                    created_at: revision.creation_time.to_rfc3339(),
                    modified_at: revision
                        .claimed_modification_time
                        .map(|time| time.to_rfc3339()),
                    size: revision.claimed_size,
                })
                .collect();
            versions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            if json {
                println!("{}", serde_json::to_string(&versions)?);
            } else {
                for version in versions {
                    println!(
                        "{}{}\t{}\t{}",
                        version.created_at,
                        if version.current { " (current)" } else { "" },
                        version
                            .size
                            .map_or_else(|| "-".to_string(), |size| size.to_string()),
                        version.uid
                    );
                }
            }
        }
        VersionsCommand::Restore { version } => {
            drive.restore_revision(parse_revision(&version)?).await?;
            println!("restored {version}");
        }
        VersionsCommand::Delete { version } => {
            drive.delete_revision(parse_revision(&version)?).await?;
            println!("deleted {version}");
        }
        VersionsCommand::Download {
            version,
            destination,
        } => {
            crate::browser::download_to_path(&drive, parse_revision(&version)?, &destination)
                .await?;
            println!(
                "{}",
                serde_json::json!({ "version": version, "path": destination })
            );
        }
    }
    Ok(())
}
