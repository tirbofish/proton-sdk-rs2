mod auth;
mod file;

use std::sync::Arc;

use futures::StreamExt;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node;
use proton_drive_sdk::utils::PotentialObject::Node;
use proton_sdk_rs2::session::ProtonAPISession;

use crate::file::FileCacheRepository;

pub struct DriveClient {
    session: ProtonAPISession,
    cache: Arc<FileCacheRepository>,
}

impl Drop for DriveClient {
    fn drop(&mut self) {
        println!("Saving cache locally to {}", self.cache.path.display());
        if let Err(e) = self.cache.persist() {
            println!("Error saving cache: {:?}", e);
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let client = DriveClient::auth().await?;
    let session = &client.session;

    let client = ProtonDriveClient::new(&session, None)?;
    let my_files = client.get_my_files_folder().await?;
    println!("Got My Files folder: {:?}", my_files.base.uid);

    // Create the destination folder
    let folder_name = format!(
        "test-folder-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
    println!("Creating folder: {}", folder_name);
    let test_folder = client
        .create_folder(my_files.base.uid.clone(), folder_name.clone(), None)
        .await?;
    println!("Created folder: {:?}", test_folder.base.uid);

    // Find a Screenshot file
    println!("Looking for a file containing 'Screenshot'...");
    let mut children_stream = client
        .enumerate_folder_children(my_files.base.uid.clone())
        .await?;

    let mut target_file = None;
    while let Some(child_result) = children_stream.next().await {
        let child = child_result?;
        if let Node(node) = child {
            if let node::Node::File(file) = node {
                if file.base.base.name.contains("Screenshot") {
                    println!(
                        "Found target file: {} ({:?})",
                        file.base.base.name, file.base.base.uid
                    );
                    target_file = Some(file);
                    break;
                }
            }
        }
    }

    if let Some(file) = target_file {
        println!(
            "Moving {} to {:?}",
            file.base.base.name, test_folder.base.uid
        );
        client
            .move_nodes(
                vec![file.base.base.uid.clone()],
                test_folder.base.uid.clone(),
            )
            .await?;
        println!("Moved successfully!");
    } else {
        println!("No file containing 'Screenshot' found.");
    }

    println!("Done!");
    Ok(())
}
