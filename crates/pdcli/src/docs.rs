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
        DocsCommand::Export {
            target,
            output,
            password,
        } => {
            let doc = client
                .open_document_with_password(&target, password.as_deref())
                .await?;
            let markdown = doc.to_markdown()?;
            match output {
                Some(path) => {
                    std::fs::write(&path, markdown)?;
                    println!("wrote\t{}", path.display());
                }
                None => println!("{markdown}"),
            }
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
    }
    Ok(())
}

fn print_opened(doc: &OpenedDocument) {
    println!("name\t{}", doc.name);
    println!("kind\t{}", doc.kind.as_str());
    println!("uid\t{}", doc.uid);
    println!("commits\t{}", doc.meta.commit_ids.len());
    if let Some(id) = &doc.commit_id {
        println!("latest\t{id}");
    }
    println!("updates\t{} ({} bytes)", doc.updates.len(), doc.yjs_bytes());
}
