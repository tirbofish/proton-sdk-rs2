use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::docs::OpenedDocument;

use crate::daemon;
use crate::flags::DocsCommand;

pub async fn run(command: DocsCommand, force_offline: bool) -> anyhow::Result<()> {
    let session = daemon::restore_session(force_offline).await?;
    let client = ProtonDriveClient::new(&session, None)?;
    match command {
        DocsCommand::Open { target, password } => {
            let doc = client
                .open_document_with_password(&target, password.as_deref())
                .await?;
            print_opened(&doc);
        }
        DocsCommand::Recents => {
            let recents = client.list_recent_documents().await?;
            if recents.is_empty() {
                println!("no recent documents");
                return Ok(());
            }
            for item in recents {
                println!(
                    "{}\t{}\t{}",
                    item.last_open_time,
                    format!("{}~{}", item.volume_id, item.link_id),
                    item.context_share_id
                );
            }
        }
        DocsCommand::Ddocs {
            target,
            api_url,
            api_key,
            password,
            open,
            once,
            interval,
        } => {
            let (api_url, api_key) = fileverse_creds(api_url, api_key)?;
            let (doc, published) = client
                .open_in_ddocs_with_password(&target, &api_url, &api_key, password.as_deref())
                .await?;
            println!("name\t{}", doc.name);
            println!("uid\t{}", doc.uid);
            println!("ddoc\t{}", published.ddoc_id);
            if let Some(status) = &published.sync_status {
                println!("sync\t{status}");
            }
            println!("edit\t{}", published.owner_url());
            if let Some(share) = published.share_url() {
                println!("share\t{share}");
            }
            let url = published.owner_url();
            if open {
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
            }

            if !once {
                println!("live\tddocs → proton every {interval}s (ctrl-c to stop)");
                watch_ddocs_to_proton(
                    &client,
                    &published.ddoc_id,
                    doc.uid.clone(),
                    &api_url,
                    &api_key,
                    interval.max(1),
                )
                .await?;
            }
        }

        DocsCommand::FromDdocs {
            ddoc_id,
            target,
            api_url,
            api_key,
            password,
        } => {
            let (api_url, api_key) = fileverse_creds(api_url, api_key)?;
            let commit_id = client
                .sync_from_ddocs(
                    &ddoc_id,
                    &target,
                    &api_url,
                    &api_key,
                    password.as_deref(),
                )
                .await?;
            println!("wrote\t{target}");
            println!("commit\t{commit_id}");
        }
    }
    Ok(())
}

fn fileverse_creds(
    api_url: Option<String>,
    api_key: Option<String>,
) -> anyhow::Result<(String, String)> {
    let api_url = api_url
        .or_else(|| std::env::var("FILEVERSE_API_URL").ok())
        .or_else(|| std::env::var("DDOCS_API_URL").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8001".into());

    let api_key = api_key
        .or_else(|| std::env::var("FILEVERSE_API_KEY").ok())
        .or_else(|| std::env::var("DDOCS_API_KEY").ok())
        .ok_or_else(|| {
            anyhow::anyhow!("missing Fileverse API key. Pass --api-key or set FILEVERSE_API_KEY.")
        })?;
    Ok((api_url, api_key))
}

fn print_opened(doc: &OpenedDocument) {
    println!("name\t{}", doc.name);
    println!("kind\t{}", doc.kind.as_str());
    println!("uid\t{}", doc.uid);
    println!("commits\t{}", doc.meta.commit_ids.len());
    if let Some(id) = &doc.commit_id {
        println!("latest\t{id}");
    }
    println!(
        "updates\t{} ({} bytes)",
        doc.updates.len(),
        doc.yjs_bytes()
    );
}

async fn watch_ddocs_to_proton(
    client: &ProtonDriveClient,
    ddoc_id: &str,
    uid: proton_drive_sdk::node::NodeUid,
    api_url: &str,
    api_key: &str,
    interval_secs: u64,
) -> anyhow::Result<()> {
    use proton_drive_sdk::docs::{fetch_ddoc, live_sync_tick, LiveSyncTick};

    let initial = fetch_ddoc(api_url, api_key, ddoc_id)
        .await
        .map(|(_, md)| md)
        .unwrap_or_default();
    let mut last_written = initial.clone();
    let mut last_seen = initial;
    let mut stable = 0u32;
    let interval = std::time::Duration::from_secs(interval_secs);

    loop {
        tokio::time::sleep(interval).await;
        let current = match fetch_ddoc(api_url, api_key, ddoc_id).await {
            Ok((_, md)) => md,
            Err(e) => {
                tracing::warn!(error = %e, "ddocs poll failed");
                continue;
            }
        };
        match live_sync_tick(&current, &mut last_seen, &last_written, &mut stable) {
            LiveSyncTick::Write => match client.write_document_markdown(uid.clone(), &current).await
            {
                Ok(commit) => {
                    println!("synced\t{commit}");
                    last_written = current;
                    stable = 0;
                }
                Err(e) => tracing::error!(error = %e, "proton write failed"),
            },
            LiveSyncTick::Changed => {
                println!("edit\t{} bytes", current.len());
            }
            LiveSyncTick::Idle => {}
        }
    }
}

