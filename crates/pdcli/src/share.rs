use proton_drive_sdk::api::share::{AbuseCategory, MemberRole};
use proton_drive_sdk::node::NodeUid;
use proton_drive_sdk::node::revision::RevisionUid;
use proton_drive_sdk::sharing::{
    ReportDirectShareAbuseSettings, ShareUrlSettings, UnshareNodeSettings,
};

use crate::{daemon, flags::ShareCommand};

pub async fn run_cli(force_offline: bool, command: ShareCommand) -> anyhow::Result<()> {
    let session = daemon::restore_session(force_offline).await?;
    let drive = proton_drive_sdk::client::ProtonDriveClient::new(&session, None)?;
    match command {
        ShareCommand::Link {
            node,
            role,
            password,
        } => {
            let uid = parse_node(&node)?;
            let url = drive
                .create_public_link(
                    uid,
                    ShareUrlSettings {
                        role: parse_role(&role)?,
                        custom_password: password,
                        expiration: None,
                    },
                )
                .await?;
            println!("{}", url.url);
        }
        ShareCommand::Status { node, json } => {
            let uid = parse_node(&node)?;
            let info = drive.get_sharing_info(uid).await?;
            let Some(info) = info else {
                if json {
                    println!("null");
                } else {
                    println!("not shared");
                }
                return Ok(());
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "members": info.members.iter().map(|member| serde_json::json!({
                            "email": &member.invitee_email,
                            "role": format!("{:?}", member.role).to_lowercase(),
                        })).collect::<Vec<_>>(),
                        "pending_invitations": info.proton_invitations.len() + info.non_proton_invitations.len(),
                        "public_link": info.url_access.as_ref().map(|url| serde_json::json!({
                            "url": &url.url,
                            "role": format!("{:?}", url.role).to_lowercase(),
                            "expires": &url.expiration_time,
                        })),
                        "editors_can_share": info.editors_can_share,
                    })
                );
            } else {
                println!("members: {}", info.members.len());
                for member in info.members {
                    println!("  {} ({:?})", member.invitee_email, member.role);
                }
                println!(
                    "pending invitations: {}",
                    info.proton_invitations.len() + info.non_proton_invitations.len()
                );
                match info.url_access {
                    Some(url) => println!("public link: {}", url.url),
                    None => println!("public link: none"),
                }
            }
        }
        ShareCommand::Remove { node } => {
            let uid = parse_node(&node)?;
            drive
                .unshare_node(
                    uid,
                    UnshareNodeSettings {
                        remove_url_access: true,
                        ..Default::default()
                    },
                )
                .await?;
            println!("public link removed");
        }
        ShareCommand::Report {
            node,
            category,
            message,
            email,
            bona_fide,
            revision,
            invitation,
        } => {
            let uid = parse_node(&node)?;
            let abuse_category = AbuseCategory::parse(&category).ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid category: {category}, must be one of: {}",
                    AbuseCategory::ALL
                        .iter()
                        .map(|value| value.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            drive
                .report_abuse(ReportDirectShareAbuseSettings {
                    node_uid: uid,
                    abuse_category,
                    bona_fide,
                    reporter_message: empty_to_none(message),
                    reporter_email: empty_to_none(email),
                    revision_uid: revision
                        .as_deref()
                        .filter(|value| !value.is_empty())
                        .map(RevisionUid::parse)
                        .transpose()
                        .map_err(|error| anyhow::anyhow!(error))?,
                    invitation_uid: empty_to_none(invitation),
                })
                .await?;
            println!("report submitted");
        }
    }
    Ok(())
}

fn empty_to_none(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn parse_node(value: &str) -> anyhow::Result<NodeUid> {
    NodeUid::parse(value).map_err(|error| anyhow::anyhow!(error))
}

fn parse_role(value: &str) -> anyhow::Result<MemberRole> {
    match value.to_ascii_lowercase().as_str() {
        "viewer" | "read" | "reader" => Ok(MemberRole::Viewer),
        "editor" | "write" => Ok(MemberRole::Editor),
        _ => anyhow::bail!("public-link role must be viewer or editor"),
    }
}
