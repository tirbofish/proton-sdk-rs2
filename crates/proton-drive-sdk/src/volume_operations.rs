use crate::client::ProtonDriveClient;
use crate::node::{DegradedNode, Node, NodeUid, VolumeTrashBatchLoader};
use crate::share_ops::ShareOperations;
use crate::utils::PotentialObject;
use std::sync::Arc;

pub struct VolumeOperations;

impl VolumeOperations {
    pub async fn enumerate_trash(
        client: &ProtonDriveClient,
    ) -> anyhow::Result<Vec<Result<Node, DegradedNode>>> {
        let volume_id = Self::get_main_volume_id(client).await?;

        let mut results = Vec::new();
        let mut page = 0;
        let mut must_try_more_results = true;
        let page_size = 500;

        while must_try_more_results {
            let response = client
                .api()
                .trash()
                .get_trash(volume_id.clone(), page_size, page)
                .await?;

            must_try_more_results = response.trash.iter().map(|x| x.link_ids.len()).sum::<usize>() == page_size as usize;

            for share_trash in response.trash {
                let share_and_key = match ShareOperations::get_share(client, share_trash.share_id).await {
                    Ok(sk) => sk,
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to get share for trash batch — skipping");
                        continue;
                    }
                };
                
                let mut batch_loader = VolumeTrashBatchLoader::new(
                    Arc::new(client.clone()),
                    volume_id.clone(),
                    share_and_key.key,
                );

                for link_id in share_trash.link_ids {
                    let uid = NodeUid::new(volume_id.clone(), link_id.clone());
                    if let Some(cached_info) = client.cache().entities().try_get_node(uid).await? {
                        results.push(match cached_info.node_provision_result {
                            PotentialObject::Node(n) => Ok(n),
                            PotentialObject::Degraded(d) => Err(d),
                        });
                    } else {
                        let batch_results = batch_loader.queue_and_try_load_batch(link_id).await?;
                        for node in batch_results {
                            results.push(match node {
                                PotentialObject::Node(n) => Ok(n),
                                PotentialObject::Degraded(d) => Err(d),
                            });
                        }
                    }
                }

                let remaining = batch_loader.load_remaining().await?;
                for node in remaining {
                    results.push(match node {
                        PotentialObject::Node(n) => Ok(n),
                        PotentialObject::Degraded(d) => Err(d),
                    });
                }
            }
            page += 1;
        }

        Ok(results)
    }

    pub async fn empty_trash(client: &ProtonDriveClient) -> anyhow::Result<()> {
        let volume_id = Self::get_main_volume_id(client).await?;
        client.api().trash().empty(volume_id).await?;
        Ok(())
    }

    async fn get_main_volume_id(
        client: &ProtonDriveClient,
    ) -> anyhow::Result<crate::volume::VolumeId> {
        let my_files = crate::node::operations::NodeOperations::get_my_files_folder(client).await?;
        Ok(my_files.base.uid.volume_id)
    }
}
