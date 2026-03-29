use crate::account::AddressId;
use crate::api::devices::{
    CreateDeviceLinkParams, CreateDeviceParams, CreateDeviceRequest, CreateDeviceShareParams,
    DeviceType, RawDeviceInfo,
};
use crate::client::ProtonDriveClient;
use crate::crypto::CryptoGenerator;
use crate::node::crypto::NodeCrypto;
use crate::node::{DegradedNode, Node, NodeUid};
use crate::pgp::PgpPrivateKey;
use crate::share::ShareId;
use crate::utils::PotentialObject;
use crate::volume::VolumeId;
use chrono::{DateTime, Utc};


/// A Computer (device) with its decrypted name.
#[derive(Debug, Clone)]
pub struct Device {
    pub device_id: String,
    pub volume_id: VolumeId,
    pub share_id: ShareId,
    /// The root folder `NodeUid` for this device.
    pub root_uid: NodeUid,
    pub device_type: DeviceType,
    pub name: String,
    pub create_time: DateTime<Utc>,
    pub last_sync_time: Option<DateTime<Utc>>,
}


pub struct DeviceOperations;

impl DeviceOperations {

    /// Fetch all devices for the authenticated user and decrypt their names.
    ///
    /// Names are stored on the root folder node (not the share), so we batch-
    /// fetch the root folder nodes to decrypt them.
    pub async fn list_devices(
        client: &ProtonDriveClient,
    ) -> anyhow::Result<Vec<Device>> {
        let resp = client.api().devices().get_devices().await?;
        resp.base.to_result()?;

        // Convert raw API entries into typed RawDeviceInfo.
        let raw: Vec<RawDeviceInfo> = resp
            .devices
            .into_iter()
            .map(RawDeviceInfo::try_from)
            .collect::<anyhow::Result<_>>()?;

        // Bootstrap each device's share key into the secret cache before
        // fetching nodes, so decryption finds the key when needed.
        for raw_info in &raw {
            if let Err(e) = crate::share_ops::ShareOperations::get_share(
                client,
                raw_info.share_id.clone(),
            )
            .await
            {
                tracing::warn!(
                    device_id = %raw_info.device_id,
                    error = %e,
                    "Failed to bootstrap computer share key — node decryption may fail"
                );
            }
        }

        // Batch-fetch the root folder node for each device to decrypt the name.
        // enumerate_nodes uses FuturesUnordered and returns results in completion
        // order, so we must match by UID rather than by position.
        let uids: Vec<NodeUid> = raw
            .iter()
            .map(|d| NodeUid::new(d.volume_id.clone(), d.root_link_id.clone()))
            .collect();

        let nodes = crate::node::operations::NodeOperations::enumerate_nodes(client, uids).await?;

        // Build a uid → name map from the (potentially out-of-order) results.
        let mut name_map: std::collections::HashMap<NodeUid, String> =
            std::collections::HashMap::with_capacity(nodes.len());
        for node_result in &nodes {
            let name = extract_name_from_node(node_result);
            let uid = match node_result {
                PotentialObject::Node(n) => n.uid().clone(),
                PotentialObject::Degraded(d) => d.uid().clone(),
            };
            name_map.insert(uid, name);
        }

        let mut devices = Vec::with_capacity(raw.len());
        for raw_info in raw.into_iter() {
            let root_uid = NodeUid::new(raw_info.volume_id.clone(), raw_info.root_link_id.clone());
            let name = name_map.remove(&root_uid).unwrap_or_else(|| "<unknown>".to_string());
            devices.push(Device {
                device_id: raw_info.device_id,
                volume_id: raw_info.volume_id.clone(),
                share_id: raw_info.share_id,
                root_uid,
                device_type: raw_info.device_type,
                name,
                create_time: raw_info.create_time,
                last_sync_time: raw_info.last_sync_time,
            });
        }

        Ok(devices)
    }

    pub async fn get_device(
        client: &ProtonDriveClient,
        device_id: &str,
    ) -> anyhow::Result<Device> {
        let devices = Self::list_devices(client).await?;
        devices
            .into_iter()
            .find(|d| d.device_id == device_id)
            .ok_or_else(|| anyhow::anyhow!("Device '{}' not found", device_id))
    }

    /// Creates a new computer entry. The server allocates a new share and
    /// root folder node; all key material is generated locally and re-encrypted
    /// under the user's primary address key.
    ///
    /// This mirrors `DevicesCryptoService.createDevice` + `DevicesAPIService.createDevice`.
    pub async fn create_device(
        client: &ProtonDriveClient,
        name: String,
        device_type: DeviceType,
    ) -> anyhow::Result<Device> {
        let default_addr = client.account().get_default_address().await?;
        let address_id = AddressId::new(default_addr.address_id.clone());

        // Primary key of the address used to sign / encrypt.
        let primary_key_index =
            usize::try_from(default_addr.primary_key_index).unwrap_or(0);
        let address_key_info = default_addr
            .keys
            .get(primary_key_index)
            .ok_or_else(|| anyhow::anyhow!("No primary address key found"))?;
        let address_key_id =
            crate::account::AddressKeyId::new(address_key_info.address_key_id.clone());

        let address_private_key = PgpPrivateKey(
            client
                .account()
                .get_address_primary_private_key(&address_id)
                .await?,
        );

        // Get the main volume ID (devices are created inside the user's main volume).
        let main_folder = crate::node::operations::NodeOperations::get_my_files_folder(client).await?;
        let volume_id = main_folder.base.uid.volume_id.clone();

        let share_key = CryptoGenerator::generate_private_key()?;
        let share_passphrase = CryptoGenerator::generate_passphrase();
        let locked_share_key =
            share_key.to_armored_private_key(Some(share_passphrase.as_bytes()))?;

        let (encrypted_share_passphrase, share_passphrase_signature, _) =
            NodeCrypto::encrypt_and_sign_passphrase(
                share_passphrase.as_bytes(),
                &address_private_key,
                &address_private_key,
            )?;

        let node_key = CryptoGenerator::generate_private_key()?;
        let node_passphrase = CryptoGenerator::generate_passphrase();
        let locked_node_key =
            node_key.to_armored_private_key(Some(node_passphrase.as_bytes()))?;

        let (encrypted_node_passphrase, node_passphrase_signature, _) =
            NodeCrypto::encrypt_and_sign_passphrase(
                node_passphrase.as_bytes(),
                &share_key,
                &address_private_key,
            )?;

        let hash_key_bytes = CryptoGenerator::generate_folder_hash_key();
        let encrypted_hash_key =
            NodeCrypto::encrypt_folder_hash_key(&node_key, &hash_key_bytes, &address_private_key)?;

        let (encrypted_name, _, _) = NodeCrypto::encrypt_and_sign_name(
            &name,
            &hash_key_bytes,
            &share_key,
            &address_private_key,
        )?;

        let request = CreateDeviceRequest {
            device: CreateDeviceParams {
                r#type: device_type,
                sync_state: 0,
            },
            share: CreateDeviceShareParams {
                address_id: address_id.clone(),
                address_key_id,
                key: locked_share_key,
                passphrase: encrypted_share_passphrase,
                passphrase_signature: share_passphrase_signature
                    .ok_or_else(|| anyhow::anyhow!("Missing passphrase signature"))?,
            },
            link: CreateDeviceLinkParams {
                name: encrypted_name,
                node_key: locked_node_key,
                node_passphrase: encrypted_node_passphrase,
                node_passphrase_signature: node_passphrase_signature
                    .ok_or_else(|| anyhow::anyhow!("Missing node passphrase signature"))?,
                node_hash_key: encrypted_hash_key,
            },
        };

        let resp = client.api().devices().create_device(request).await?;
        resp.base.to_result()?;

        // Bootstrap the new device's share key into the secret cache immediately
        // so that subsequent create_folder / get_secrets calls against the device
        // root can decrypt the node key without a second list_devices round-trip.
        if let Err(e) = crate::share_ops::ShareOperations::get_share(
            client,
            resp.device.share_id.clone(),
        )
        .await
        {
            tracing::warn!(
                share_id = %resp.device.share_id.raw(),
                error = %e,
                "Failed to bootstrap new device share key — first create_folder may fail"
            );
        }

        Ok(Device {
            device_id: resp.device.device_id,
            volume_id: volume_id.clone(),
            share_id: resp.device.share_id,
            root_uid: NodeUid::new(volume_id, resp.device.link_id),
            device_type,
            name,
            create_time: Utc::now(),
            last_sync_time: None,
        })
    }

    //  Rename device 

    /// Renames a device by renaming its root folder node.
    ///
    /// If the device still carries the old "deprecated" share-level name it is
    /// cleared first.
    pub async fn rename_device(
        client: &ProtonDriveClient,
        device_id: &str,
        new_name: String,
    ) -> anyhow::Result<Device> {
        let resp = client.api().devices().get_devices().await?;
        resp.base.to_result()?;

        let raw = resp
            .devices
            .into_iter()
            .find(|e| e.device.device_id == device_id)
            .ok_or_else(|| anyhow::anyhow!("Device '{}' not found", device_id))?;
        let has_deprecated_name = raw
            .share
            .name
            .as_ref()
            .map(|n| !n.is_empty())
            .unwrap_or(false);

        if has_deprecated_name {
            // Best-effort: clear the deprecated share-level name.
            if let Err(e) = client.api().devices().clear_device_share_name(device_id).await {
                tracing::warn!(device_id, error = %e, "Failed to clear deprecated device share name");
            }
        }

        let root_uid = NodeUid::new(
            raw.device.volume_id.clone(),
            raw.share.link_id.clone(),
        );

        // Rename the root node (rename_node with none for media_type).
        crate::node::operations::NodeOperations::rename(client, root_uid.clone(), new_name.clone(), None).await?;

        Ok(Device {
            device_id: device_id.to_string(),
            volume_id: raw.device.volume_id,
            share_id: raw.share.share_id,
            root_uid,
            device_type: raw.device.r#type,
            name: new_name,
            create_time: {
                use chrono::TimeZone;
                Utc.timestamp_opt(raw.device.create_time, 0)
                    .single()
                    .ok_or_else(|| anyhow::anyhow!("Invalid create_time"))?
            },
            last_sync_time: raw.device.last_sync_time.and_then(|t| {
                use chrono::TimeZone;
                Utc.timestamp_opt(t, 0).single()
            }),
        })
    }

    // Delete device 
    pub async fn delete_device(
        client: &ProtonDriveClient,
        device_id: &str,
    ) -> anyhow::Result<()> {
        client.api().devices().delete_device(device_id).await
    }
}

fn extract_name_from_node(node: &PotentialObject<Node, DegradedNode>) -> String {
    match node {
        PotentialObject::Node(n) => n.base().name.clone(),
        PotentialObject::Degraded(d) => {
            let name_pot = match d {
                DegradedNode::Folder(n) | DegradedNode::Album(n) => &n.base.name,
                DegradedNode::File(n) | DegradedNode::Photo(n) => &n.base.name,
            };
            match name_pot {
                PotentialObject::Node(s) => s.clone(),
                PotentialObject::Degraded(_) => "<unavailable>".to_string(),
            }
        }
    }
}
