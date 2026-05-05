pub mod download;
pub mod upload;

use crate::api::file::thumbnail::ThumbnailBlock;
use crate::client::ProtonDriveClient;
use crate::error::ProtonDriveError;
use crate::links::LinkId;
use crate::meta::AdditionalMetadataProperty;
use crate::node::DegradedNodeBase;
use crate::node::revision::{DegradedRevision, Revision};
use crate::node::{DegradedNode, DegradedNodeSecrets, Node, NodeSecrets};
use crate::node::{NodeUid, thumbnail::ThumbnailType};
use crate::pgp::PgpSessionKey;
use crate::protobuf::ThumbnailHeader;
use crate::share::ShareId;
use crate::utils::PotentialObject;
use crate::volume::VolumeId;
use anyhow::Context;
use chrono::{DateTime, Utc};
use futures::stream::{FuturesUnordered, StreamExt};
use proton_rpgp::pgp::crypto::sym::SymmetricKeyAlgorithm;
use proton_rpgp::{DataEncoding, Decryptor, SessionKey};
use std::collections::{HashMap, HashSet};
use std::pin::Pin;

pub struct FileOperations;

/// Helper struct to store file node info for thumbnail enumeration.
#[derive(Clone, Debug)]
struct FileNodeInfo {
    uid: NodeUid,
    thumbnails: Vec<ThumbnailHeader>,
}

impl FileOperations {
    pub async fn get_secrets(
        client: &ProtonDriveClient,
        file_uid: NodeUid,
    ) -> anyhow::Result<FileSecrets> {
        if let Some(secrets) = client
            .cache()
            .secrets()
            .try_get_file_secrets(file_uid.clone())
            .await?
        {
            return secrets.result().map_err(|e| anyhow::anyhow!(e.to_string()));
        }

        let metadata_result =
            crate::node::operations::NodeOperations::get_node_metadata(client, file_uid).await?;
        let metadata = metadata_result.result()?;

        match metadata.try_get_file_else_folder() {
            Ok((_, secrets)) => Ok(secrets),
            Err(_) => anyhow::bail!("Expected file, got folder"),
        }
    }

    pub async fn enumerate_thumbnails(
        client: &ProtonDriveClient,
        file_uids: Vec<NodeUid>,
        thumbnail_type: ThumbnailType,
    ) -> anyhow::Result<Vec<FileThumbnail>> {
        let mut all_results: Vec<FileThumbnail> = Vec::new();

        // Group file UIDs by volume ID for batched API calls
        let mut volume_groups: HashMap<VolumeId, Vec<String>> = HashMap::new();
        for uid in &file_uids {
            volume_groups
                .entry(uid.volume_id.clone())
                .or_default()
                .push(uid.link_id.raw().to_string());
        }

        for (volume_id, link_ids) in volume_groups {
            let link_id_set: HashSet<String> = link_ids.into_iter().collect();
            let node_uids: Vec<NodeUid> = link_id_set
                .iter()
                .map(|link_id| NodeUid {
                    volume_id: volume_id.clone(),
                    link_id: LinkId::new(link_id.clone()),
                })
                .collect();

            // Enumerate the nodes to get their metadata
            let node_results =
                crate::node::operations::NodeOperations::enumerate_nodes(client, node_uids.clone())
                    .await?;

            let mut errors: Vec<FileThumbnail> = Vec::new();
            let mut thumbnail_id_to_info: HashMap<String, FileNodeInfo> = HashMap::new();
            let mut processed_link_ids: HashSet<String> = HashSet::new();

            for node_result in node_results {
                let file_node_info = match &node_result {
                    PotentialObject::Node(node) => {
                        processed_link_ids.insert(node.uid().link_id.raw().to_string());

                        match node {
                            Node::File(file_node) | Node::Photo(file_node) => Some(FileNodeInfo {
                                uid: file_node.base.base.uid.clone(),
                                thumbnails: file_node.active_revision.thumbnails.clone(),
                            }),
                            Node::Folder(_) | Node::Album(_) => {
                                errors.push(FileThumbnail {
                                    file_uid: node.uid().clone(),
                                    result: PotentialObject::Degraded(
                                        ProtonDriveError::InternalError(
                                            "Node is not a file".to_string(),
                                        ),
                                    ),
                                });
                                None
                            }
                        }
                    }
                    PotentialObject::Degraded(degraded_node) => {
                        processed_link_ids.insert(degraded_node.uid().link_id.raw().to_string());

                        match degraded_node {
                            DegradedNode::File(degraded_file)
                            | DegradedNode::Photo(degraded_file) => {
                                if let Some(ref degraded_revision) = degraded_file.active_revision {
                                    if degraded_revision.can_decrypt {
                                        Some(FileNodeInfo {
                                            uid: degraded_file.base.uid.clone(),
                                            thumbnails: degraded_revision.thumbnails.clone(),
                                        })
                                    } else {
                                        let error_msg = if let Some(ref content_author) =
                                            degraded_revision.content_author
                                        {
                                            match content_author {
                                                PotentialObject::Degraded(e) => {
                                                    format!(
                                                        "Cannot decrypt degraded file: {}",
                                                        e.message
                                                    )
                                                }
                                                _ => "Cannot decrypt degraded file".to_string(),
                                            }
                                        } else {
                                            "Cannot decrypt degraded file".to_string()
                                        };
                                        errors.push(FileThumbnail {
                                            file_uid: degraded_file.base.uid.clone(),
                                            result: PotentialObject::Degraded(
                                                ProtonDriveError::InternalError(error_msg),
                                            ),
                                        });
                                        None
                                    }
                                } else {
                                    errors.push(FileThumbnail {
                                        file_uid: degraded_file.base.uid.clone(),
                                        result: PotentialObject::Degraded(
                                            ProtonDriveError::InternalError(
                                                "File has no active revision".to_string(),
                                            ),
                                        ),
                                    });
                                    None
                                }
                            }
                            DegradedNode::Folder(_) | DegradedNode::Album(_) => {
                                errors.push(FileThumbnail {
                                    file_uid: degraded_node.uid().clone(),
                                    result: PotentialObject::Degraded(
                                        ProtonDriveError::InternalError(
                                            "Node is not a file".to_string(),
                                        ),
                                    ),
                                });
                                None
                            }
                        }
                    }
                };

                if let Some(info) = file_node_info {
                    // Check for thumbnails of the requested type
                    let thumbnail_type_i32 = thumbnail_type as i32;
                    if info.thumbnails.is_empty() {
                        errors.push(FileThumbnail {
                            file_uid: info.uid.clone(),
                            result: PotentialObject::Degraded(ProtonDriveError::InternalError(
                                "Node has no thumbnails".to_string(),
                            )),
                        });
                    } else if !info
                        .thumbnails
                        .iter()
                        .any(|t| t.r#type == thumbnail_type_i32)
                    {
                        errors.push(FileThumbnail {
                            file_uid: info.uid.clone(),
                            result: PotentialObject::Degraded(ProtonDriveError::InternalError(
                                format!("Node has no thumbnail of type {:?}", thumbnail_type),
                            )),
                        });
                    } else {
                        // Add all thumbnails of the requested type to the map
                        for thumbnail in &info.thumbnails {
                            if thumbnail.r#type == thumbnail_type_i32 {
                                thumbnail_id_to_info.insert(thumbnail.id.clone(), info.clone());
                            }
                        }
                    }
                }
            }

            // Add errors for nodes that were not found
            for link_id in &link_id_set {
                if !processed_link_ids.contains(link_id) {
                    errors.push(FileThumbnail {
                        file_uid: NodeUid {
                            volume_id: volume_id.clone(),
                            link_id: LinkId::new(link_id.clone()),
                        },
                        result: PotentialObject::Degraded(ProtonDriveError::NotFound),
                    });
                }
            }

            // Add all errors to results
            all_results.extend(errors);

            if thumbnail_id_to_info.is_empty() {
                continue;
            }

            // Get thumbnail block URLs from the API
            let thumbnail_ids: Vec<String> = thumbnail_id_to_info.keys().cloned().collect();
            let response = client
                .api()
                .files()
                .get_thumbnail_blocks(volume_id.clone(), thumbnail_ids.clone())
                .await
                .context("Failed to get thumbnail blocks from server")?;

            let processed_thumbnail_ids: HashSet<String> = response
                .blocks
                .iter()
                .map(|b| b.thumbnail_id.clone())
                .collect();

            // Download thumbnails in parallel with concurrency limit
            const CONCURRENCY: usize = 8;

            type ThumbnailFuture = Pin<Box<dyn std::future::Future<Output = FileThumbnail> + Send>>;

            let mut in_flight: FuturesUnordered<ThumbnailFuture> = FuturesUnordered::new();
            let mut block_iter = response.blocks.into_iter();

            // Seed the initial concurrent batch
            for block in block_iter.by_ref().take(CONCURRENCY) {
                let info = thumbnail_id_to_info.get(&block.thumbnail_id).cloned();
                if let Some(info) = info {
                    let c = client.clone();
                    in_flight.push(Box::pin(Self::download_thumbnail(c, info, block)));
                }
            }

            while let Some(result) = in_flight.next().await {
                all_results.push(result);

                // Keep the pipeline full while there are more blocks
                if let Some(block) = block_iter.next() {
                    let info = thumbnail_id_to_info.get(&block.thumbnail_id).cloned();
                    if let Some(info) = info {
                        let c = client.clone();
                        in_flight.push(Box::pin(Self::download_thumbnail(c, info, block)));
                    }
                }
            }

            // Add errors for thumbnails that were not returned by the server
            for thumbnail_id in &thumbnail_ids {
                if !processed_thumbnail_ids.contains(thumbnail_id) {
                    if let Some(info) = thumbnail_id_to_info.get(thumbnail_id) {
                        all_results.push(FileThumbnail {
                            file_uid: info.uid.clone(),
                            result: PotentialObject::Degraded(ProtonDriveError::NotFound),
                        });
                    }
                }
            }
        }

        Ok(all_results)
    }

    /// Download and decrypt a single thumbnail block.
    async fn download_thumbnail(
        client: ProtonDriveClient,
        info: FileNodeInfo,
        block: ThumbnailBlock,
    ) -> FileThumbnail {
        match Self::download_thumbnail_inner(&client, &info, &block).await {
            Ok(data) => FileThumbnail {
                file_uid: info.uid,
                result: PotentialObject::Node(data),
            },
            Err(e) => FileThumbnail {
                file_uid: info.uid,
                result: PotentialObject::Degraded(ProtonDriveError::InternalError(format!(
                    "Failed to download thumbnail: {}",
                    e
                ))),
            },
        }
    }

    async fn download_thumbnail_inner(
        client: &ProtonDriveClient,
        info: &FileNodeInfo,
        block: &ThumbnailBlock,
    ) -> anyhow::Result<Vec<u8>> {
        // Get file secrets for decryption
        let secrets = Self::get_secrets(client, info.uid.clone())
            .await
            .context("Failed to get file secrets for thumbnail")?;

        // Download the encrypted thumbnail blob
        let response = client
            .api()
            .storage()
            .get_blob_stream(&block.bare_url, &block.token)
            .await
            .context("Failed to download thumbnail blob")?;

        let blob_bytes = response
            .bytes()
            .await
            .context("Failed to read thumbnail blob bytes")?;

        // Decrypt the thumbnail using the content key
        let alg = SymmetricKeyAlgorithm::from(secrets.content_key.algorithm);
        let sk = SessionKey::new(&secrets.content_key.key, alg);
        let result = Decryptor::default()
            .with_session_key(sk)
            .decrypt(&blob_bytes, DataEncoding::Auto)
            .context("Failed to decrypt thumbnail")?;

        Ok(result.data)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct DegradedFileNode {
    #[serde(flatten)]
    pub base: DegradedNodeBase,
    pub media_type: String,
    pub active_revision: Option<DegradedRevision>,
    pub total_storage_quota_usage: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DegradedFileSecrets {
    pub base: DegradedNodeSecrets,
    pub content_key: Option<PgpSessionKey>,
}

#[derive(Debug, Clone)]
pub struct DegradedFileMetadata {
    pub node: DegradedFileNode,
    pub secrets: DegradedFileSecrets,
    pub membership_share_id: Option<ShareId>,
    pub name_hash_digest: Vec<u8>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct FileContentDigests {
    pub sha1: Option<Vec<u8>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileOrFileDraftNode {
    #[serde(flatten)]
    pub base: crate::node::NodeBase,
    /// The file type in the format of a MIME type
    pub media_type: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileNode {
    #[serde(flatten)]
    pub base: FileOrFileDraftNode,
    pub active_revision: Revision,
    pub total_size_on_cloud_storage: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileDraftNode {
    #[serde(flatten)]
    pub base: FileOrFileDraftNode,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileSecrets {
    pub base: NodeSecrets,
    pub content_key: PgpSessionKey,
}

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub node: FileNode,
    pub secrets: FileSecrets,
    pub membership_share_id: Option<ShareId>,
    pub name_hash_digest: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct FileThumbnail {
    pub file_uid: NodeUid,
    pub result: PotentialObject<Vec<u8>, ProtonDriveError>,
}

#[derive(Debug, Clone)]
pub struct FileUploadMetadata {
    pub last_modification_time: Option<DateTime<Utc>>,
    pub additional_metadata: Option<Vec<AdditionalMetadataProperty>>,
}
