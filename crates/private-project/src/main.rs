mod auth;
mod file;

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use proton_drive::client::ProtonDriveClient;
use proton_drive::utils::PotentialObject::Node;
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

    let new_name = format!("{}-renamed", folder_name);
    println!("Renaming folder to: {}", new_name);
    client
        .rename_node(test_folder.base.uid.clone(), new_name.clone(), None)
        .await?;
    println!("Renamed folder successfully");

    println!("Enumerating children of My Files:");
    let mut children_stream = client
        .enumerate_folder_children(my_files.base.uid.clone())
        .await?;

    let mut found = false;
    while let Some(child_result) = children_stream.next().await {
        let child = child_result?;
        if let Node(node) = child {
            println!("{} - {}", node.ty(), node.base().name);
            if node.base().name == new_name {
                println!("Found renamed folder: {:?}", node.base().uid);
                found = true;
            }
        }
    }
    if !found {
        println!("Error: Renamed folder not found in children listing!");
    }

    println!("Trashing folder: {:?}", test_folder.base.uid);
    let trash_results = client
        .trash_nodes(vec![test_folder.base.uid.clone()])
        .await?;
    for (uid, result) in trash_results {
        match result {
            Ok(_) => println!("Successfully trashed node: {:?}", uid),
            Err(e) => println!("Failed to trash node: {:?}, error: {}", uid, e),
        }
    }

    println!("Looking for a file containing 'Screenshot'...");
    let mut children_stream = client
        .enumerate_folder_children(my_files.base.uid.clone())
        .await?;

    let mut target_file = None;
    while let Some(child_result) = children_stream.next().await {
        let child = child_result?;
        if let Node(node) = child {
            if let proton_drive::node::Node::File(file) = node {
                if file.base.base.name.contains("Screenshot") {
                    println!(
                        "Found target file: {} ({:?})",
                        file.base.base.name, file.base.base.uid
                    );
                    target_file = Some(file.clone());
                    break;
                } else {
                    println!("Miss! [{:?}]", file.base.base.name);
                }
            }
        }
    }

    if let Some(file) = target_file {
        println!("Downloading file: {}", file.base.base.name);

        let file_name = PathBuf::from(file.base.base.name);
        println!("Downloading content to local file: {}", file_name.display());

        client
            .download_to_file(
                file.base.base.uid.clone(),
                &file_name,
                Box::new(|current, total| {
                    println!("Downloaded {}/{} bytes", current, total);
                }),
            )
            .await?;

        println!("Downloaded successfully");

        println!("Reading from file: {}", file_name.display());
        let read_content = std::fs::read(&file_name)?;
        println!("Read {} bytes from file", read_content.len());

        println!("file media type: {}", file.base.media_type);

        let upload_name = format!(
            "uploaded-file-{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs()
        );
        let upload_path = std::path::PathBuf::from(&upload_name);
        std::fs::copy(&file_name, &upload_path)?;

        println!("Uploading file: {} to root", upload_name);
        client
            .upload_file(
                &upload_path,
                my_files.base.uid.clone(),
                false,
                Box::new(|current, total| {
                    println!("Uploaded {}/{} bytes", current, total);
                }),
            )
            .await?;
        println!("Uploaded successfully to root as {}", upload_name);
    } else {
        println!("No file containing 'Screenshot' found to test download/upload.");
    }

    println!("All tests completed!");

    Ok(())
}
