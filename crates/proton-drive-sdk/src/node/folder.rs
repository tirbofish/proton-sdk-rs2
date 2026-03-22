use crate::client::ProtonDriveClient;
use crate::node::crypto::NodeCrypto;
use crate::node::{
    DegradedNode, DtoToMetadataConverter, Node, NodeBase, NodeMetadataResult, NodeSecrets, NodeUid,
    OwnedBy,
};
use crate::pgp::PgpPrivateKey;
use crate::utils::PotentialObject;
use chrono::Utc;
use std::sync::Arc;

pub struct FolderOperations;

impl FolderOperations {
    pub async fn create(
        client: &ProtonDriveClient,
        parent_uid: NodeUid,
        name: String,
        _last_modification_time: Option<std::time::SystemTime>,
    ) -> anyhow::Result<FolderNode> {
        let membership_address = client
            .account()
            .get_address_primary_private_key(&crate::account::AddressId::new(
                client
                    .account()
                    .get_default_address()
                    .await?
                    .address_id
                    .clone(),
            ))
            .await?;

        let parent_secrets = Self::get_secrets(client, parent_uid.clone()).await?;
        let parent_key = parent_secrets.base.key;

        let folder_key = crate::crypto::CryptoGenerator::generate_private_key()?;
        let folder_passphrase = crate::crypto::CryptoGenerator::generate_passphrase();
        let locked_folder_key =
            folder_key.to_armored_private_key(Some(folder_passphrase.as_bytes()))?;

        let folder_secrets = FolderSecrets {
            base: NodeSecrets {
                key: folder_key,
                passphrase_session_key: crate::crypto::CryptoGenerator::generate_session_key(),
                passphrase_pgp_session_key: None,
                name_session_key: crate::crypto::CryptoGenerator::generate_session_key(),
                passphrase_for_anonymous_move: None,
            },
            hash_key: crate::crypto::CryptoGenerator::generate_folder_hash_key().to_vec(),
        };

        let (encrypted_passphrase, passphrase_signature, _passphrase_session_key) =
            NodeCrypto::encrypt_and_sign_passphrase(
                folder_passphrase.as_bytes(),
                &parent_key,
                &PgpPrivateKey(membership_address.clone()),
            )?;

        let (encrypted_name, name_hash_digest, _name_session_key) =
            NodeCrypto::encrypt_and_sign_name(
                &name,
                &parent_secrets.hash_key,
                &parent_key,
                &PgpPrivateKey(membership_address.clone()),
            )?;

        let encrypted_hash_key = NodeCrypto::encrypt_folder_hash_key(
            &folder_secrets.base.key,
            &folder_secrets.hash_key,
            &PgpPrivateKey(membership_address.clone()),
        )?;

        let request = crate::api::folder::FolderCreationRequest {
            base: crate::api::node::NodeCreationRequest {
                name: encrypted_name,
                name_hash_digest,
                parent_link_id: parent_uid.link_id.clone(),
                passphrase: encrypted_passphrase,
                passphrase_signature: passphrase_signature
                    .ok_or_else(|| anyhow::anyhow!("Passphrase signature missing"))?,
                key: locked_folder_key,
            },
            hash_key: encrypted_hash_key,
            signature_email_address: client
                .account()
                .get_default_address()
                .await?
                .email_address
                .clone(),
            extended_attributes: None,
        };

        let response = client
            .api()
            .folders()
            .create_folder(parent_uid.volume_id.clone(), request)
            .await?;

        let node = FolderNode {
            base: NodeBase {
                uid: NodeUid::new(
                    parent_uid.volume_id.clone(),
                    response.folder_id.value.clone(),
                ),
                parent_uid: Some(parent_uid),
                name: name.clone(),
                owned_by: Some(OwnedBy {
                    email: Some(
                        client
                            .account()
                            .get_default_address()
                            .await?
                            .email_address
                            .clone(),
                    ),
                    organisation: None,
                }),
                creation_time: Utc::now(),
                trash_time: None,
                ..NodeBase::default()
            },
        };

        // Cache the newly created folder secrets
        client
            .cache()
            .secrets()
            .set_folder_secrets(node.base.uid.clone(), PotentialObject::Node(folder_secrets))
            .await?;

        Ok(node)
    }

    pub async fn enumerate_children(
        client: &ProtonDriveClient,
        folder_uid: NodeUid,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        let volume_id = folder_uid.volume_id.clone();
        let mut anchor_id = None;
        let mut must_try_more_results = true;
        let mut results = Vec::new();

        let folder_secrets = Self::get_secrets(client, folder_uid.clone()).await?;
        let mut batch_loader = crate::node::NodeBatchLoader::new(
            Arc::new(client.clone()),
            volume_id.clone(),
            Some(folder_secrets.base.key),
        );

        while must_try_more_results {
            let response = client
                .api()
                .folders()
                .get_children(
                    volume_id.clone(),
                    folder_uid.link_id.clone(),
                    anchor_id.clone(),
                )
                .await?;

            must_try_more_results = response.more_results_exist;
            if let Some(last_id) = response.link_ids.last() {
                anchor_id = Some(last_id.clone());
            }

            for child_link_id in response.link_ids {
                // Check cache first (simplified for now)
                let batch_results = batch_loader.queue_and_try_load_batch(child_link_id).await?;
                results.extend(batch_results);
            }
        }

        let remaining = batch_loader.load_remaining().await?;
        results.extend(remaining);

        Ok(results)
    }

    /// Streaming version of [`enumerate_children`]: fetches and decrypts children
    /// page-by-page, sending each item through `tx` as soon as it is ready.
    /// Intended to be called from inside a `tokio::spawn` so the caller gets
    /// items without waiting for the whole listing to complete.
    pub async fn enumerate_children_to_channel(
        client: ProtonDriveClient,
        folder_uid: NodeUid,
        tx: tokio::sync::mpsc::Sender<anyhow::Result<PotentialObject<Node, DegradedNode>>>,
    ) {
        let result: anyhow::Result<()> = async {
            let volume_id = folder_uid.volume_id.clone();
            let folder_secrets = Self::get_secrets(&client, folder_uid.clone()).await?;
            let mut batch_loader = crate::node::NodeBatchLoader::new(
                Arc::new(client.clone()),
                volume_id.clone(),
                Some(folder_secrets.base.key),
            );

            let mut anchor_id = None;
            let mut must_try_more_results = true;
            while must_try_more_results {
                let response = client
                    .api()
                    .folders()
                    .get_children(
                        volume_id.clone(),
                        folder_uid.link_id.clone(),
                        anchor_id.clone(),
                    )
                    .await?;

                must_try_more_results = response.more_results_exist;
                if let Some(last) = response.link_ids.last() {
                    anchor_id = Some(last.clone());
                }
                for child_id in response.link_ids {
                    let batch = batch_loader.queue_and_try_load_batch(child_id).await?;
                    for item in batch {
                        if tx.send(Ok(item)).await.is_err() {
                            return Ok(()); // receiver dropped — stop silently
                        }
                    }
                }
            }

            let remaining = batch_loader.load_remaining().await?;
            for item in remaining {
                if tx.send(Ok(item)).await.is_err() {
                    return Ok(());
                }
            }
            Ok(())
        }
        .await;

        if let Err(e) = result {
            let _ = tx.send(Err(e)).await;
        }
    }

    pub async fn get_secrets(
        client: &ProtonDriveClient,
        folder_uid: crate::node::NodeUid,
    ) -> anyhow::Result<FolderSecrets> {
        if let Some(secrets) = client
            .cache()
            .secrets()
            .try_get_folder_secrets(folder_uid.clone())
            .await?
        {
            return secrets.result().map_err(|e| anyhow::anyhow!(e.to_string()));
        }

        let metadata_result: NodeMetadataResult =
            DtoToMetadataConverter::get_fresh_node_metadata(client, folder_uid.clone(), None)
                .await?;

        metadata_result
            .try_get_folder_secrets_else_error()
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FolderNode {
    #[serde(flatten)]
    pub base: crate::node::NodeBase,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct DegradedFolderNode {
    pub base: crate::node::DegradedNodeBase,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FolderSecrets {
    pub base: NodeSecrets,
    pub hash_key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct FolderMetadata {
    pub node: FolderNode,
    pub secrets: FolderSecrets,
    pub membership_share_id: Option<crate::share::ShareId>,
    pub name_hash_digest: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct DegradedFolderMetadata {
    pub node: DegradedFolderNode,
    pub secrets: DegradedFolderSecrets,
    pub membership_share_id: Option<crate::share::ShareId>,
    pub name_hash_digest: Vec<u8>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DegradedFolderSecrets {
    pub base: crate::node::secrets::DegradedNodeSecrets,
    pub hash_key: Option<Vec<u8>>,
}
