use futures::StreamExt;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::NodeUid;
use proton_drive_sdk::utils::PotentialObject;
use proton_drive_sdk::volume::VolumeId;
use std::sync::Arc;

use crate::index::NodeIndex;

/// Drive event types defined by the Proton Drive API.
const EVENT_TYPE_DELETE: i32 = 1;
#[allow(dead_code)]
const EVENT_TYPE_CREATE: i32 = 2;
#[allow(dead_code)]
const EVENT_TYPE_UPDATE: i32 = 3;

pub fn spawn_event_watcher(
    drive: Arc<ProtonDriveClient>,
    index: Arc<NodeIndex>,
    volume_id: VolumeId,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut event_id = match drive.get_volume_latest_event_id(volume_id.clone()).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Failed to get initial event ID: {e}");
                return;
            }
        };

        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

            let resp = match drive.poll_volume_events(volume_id.clone(), &event_id).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Event poll failed: {e}");
                    continue;
                }
            };

            event_id = resp.event_id;

            if resp.refresh {
                // Server requested a full refresh — drop all cached "fully indexed" markers
                // so the next `ls` will re-fetch each folder from the server.
                tracing::info!("Full volume refresh requested by server — clearing indexed markers");
                index.unmark_all_indexed();
            }

            for event in &resp.events {
                let uid = NodeUid::new(volume_id.clone(), event.link.link_id.clone());

                if event.event_type == EVENT_TYPE_DELETE
                    || event.link.is_trashed
                {
                    // Node was deleted or permanently trashed — remove it from the index.
                    // `remove` also cleans up the parent's children list.
                    index.remove(&uid);
                } else {
                    // Create or update: re-fetch this specific node and update its entry.
                    // This way the parent folder's indexed status stays valid and subsequent
                    // `ls` calls see fresh metadata without re-fetching the whole folder.
                    let drive_clone = drive.clone();
                    let index_clone = index.clone();
                    let parent_uid = event.link.parent_link_id.as_ref()
                        .map(|pid| NodeUid::new(volume_id.clone(), pid.clone()));
                    tokio::spawn(async move {
                        match drive_clone.get_node(uid).await {
                            Ok(node) => index_clone.insert_node(&node, parent_uid),
                            Err(e) => tracing::warn!("Failed to re-fetch node after event: {e}"),
                        }
                    });
                }
            }

            if resp.more {
                continue;
            }
        }
    })
}

#[allow(dead_code)]
pub fn spawn_index_walker(
    drive: Arc<ProtonDriveClient>,
    index: Arc<NodeIndex>,
    root_uid: NodeUid,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(root_uid);

        while let Some(folder_uid) = queue.pop_front() {
            if index.is_indexed(&folder_uid) {
                continue;
            }

            let stream = match drive.enumerate_folder_children(folder_uid.clone()).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Index walker failed on {folder_uid}: {e}");
                    continue;
                }
            };

            tokio::pin!(stream);
            while let Some(result) = stream.next().await {
                match result {
                    Ok(ref node) => {
                        let (uid, is_folder) = match node {
                            PotentialObject::Node(n) => {
                                use proton_drive_sdk::node::Node;
                                let is_folder =
                                    matches!(n, Node::Folder(_) | Node::Album(_));
                                (n.uid().clone(), is_folder)
                            }
                            PotentialObject::Degraded(d) => {
                                use proton_drive_sdk::node::DegradedNode;
                                let is_folder =
                                    matches!(d, DegradedNode::Folder(_) | DegradedNode::Album(_));
                                (d.uid().clone(), is_folder)
                            }
                        };
                        index.insert_node(node, Some(folder_uid.clone()));
                        if is_folder {
                            queue.push_back(uid);
                        }
                    }
                    Err(e) => tracing::warn!("Index walker: degraded item: {e}"),
                }
            }

            index.mark_indexed(&folder_uid);
        }

        tracing::info!("Index walk complete");
    })
}
