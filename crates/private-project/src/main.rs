mod auth;
mod file;

use std::path::PathBuf;
use std::sync::Arc;

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
    let _ = env_logger::try_init();

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

    // issue to fix: on each iteration this should be displaying the info of a new child, not everything all grouped up together
    println!("Enumerating children of My Files:");
    let children = match client
        .enumerate_folder_children(my_files.base.uid.clone())
        .await
    {
        Ok(c) => c,
        Err(e) => {
            println!("Warning: Failed to enumerate some children: {}", e);
            vec![client.get_node(test_folder.base.uid.clone()).await?]
        }
    };
    let mut found = false;
    for child in &children {
        if let Node(node) = child {
            let node: &proton_drive::node::Node = node;
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
    let children = match client
        .enumerate_folder_children(my_files.base.uid.clone())
        .await
    {
        Ok(c) => c,
        Err(_) => Vec::new(),
    };

    let mut target_file = None;
    for child in &children {
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

    // issues: this is directly using the client api(). most, if not all should be done with the ProtonDriveClient.
    if let Some(file) = target_file {
        println!("Downloading file: {}", file.base.base.name);
        let revision = file.active_revision;

        let mut file_content = Vec::new();

        let revision_details = client
            .api()
            .files()
            .get_revision(
                revision.uid.node_uid.volume_id.clone(),
                revision.uid.node_uid.link_id.clone(),
                revision.uid.revision_id.clone(),
                None,
                None,
                false,
            )
            .await?;

        let secrets = proton_drive::node::file::FileOperations::get_secrets(
            &client,
            file.base.base.uid.clone(),
        )
        .await?;
        let content_key = secrets.content_key;

        // issue: again, this should be accessed through the client/have a way to access through the ProtonDriveClient, not the API directly.
        for block in revision_details.revision.blocks {
            println!("Downloading block: {}", block.index);
            let response = client
                .api()
                .storage()
                .get_blob_stream(&block.bare_url, &block.token)
                .await?;
            let encrypted_data = response.bytes().await?;
            println!(
                "Downloaded block {} as {} bytes",
                block.index,
                encrypted_data.len()
            );

            let sk = content_key.to_rpgp_sk()?;
            let result = proton_rpgp::Decryptor::default()
                .with_session_key(sk)
                .decrypt(&encrypted_data, proton_rpgp::DataEncoding::Auto)?;

            file_content.extend_from_slice(&result.data);
        }
        println!("Downloaded {} bytes", file_content.len());

        let file_name = PathBuf::from(file.base.base.name);

        println!(
            "Saving decrypted content to local file: {}",
            file_name.display()
        );
        std::fs::write(&file_name, &file_content)?;
        println!("Saved successfully");

        println!("Reading from file: {}", file_name.display());
        let read_content = std::fs::read(&file_name)?;
        println!("Read {} bytes from file", read_content.len());

        if read_content != file_content {
            anyhow::bail!("Read content does not match original content!");
        }

        let new_folder_name = format!(
            "upload-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs()
        );
        println!("Creating new folder for re-upload: {}", new_folder_name);
        let upload_folder = client
            .create_folder(my_files.base.uid.clone(), new_folder_name, None)
            .await?;

        println!("file media type: {}", file.base.media_type);

        // this hasnt been implemented yet.
        client
            .upload_file(
                &file_name,
                upload_folder.base.uid,
                false,
                Box::new(|current, total| {
                    println!("Uploaded {}/{} bytes", current, total);
                }),
            )
            .await?;
    } else {
        println!("No file containing 'Screenshot' found to test download/upload.");
    }

    println!("All tests completed!");

    Ok(())
}
