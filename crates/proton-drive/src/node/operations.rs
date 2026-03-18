use crate::client::ProtonDriveClient;
use crate::node::{
    DegradedNode, DtoToMetadataConverter, Node, NodeAndSecrets, NodeMetadata, NodeMetadataResult,
    NodeUid,
};
use crate::pgp::PgpPrivateKey;
use crate::share_ops::ShareOperations;
use crate::utils::PotentialObject;
use hmac::{Hmac, KeyInit, Mac};
use proton_rpgp::DataEncoding;
use proton_sdk_rs2::protobuf::Address;
use sha2::Sha256;
use std::collections::HashMap;

type HmacSha256 = Hmac<Sha256>;

pub struct NodeOperations;

impl NodeOperations {
    pub async fn get_my_files_folder(
        client: &ProtonDriveClient,
    ) -> anyhow::Result<crate::node::folder::FolderNode> {
        if let Some(share_id) = client
            .cache()
            .entities()
            .try_get_my_files_share_id()
            .await?
        {
            let share_and_key = ShareOperations::get_share(client, share_id).await?;
            let root_folder_id = share_and_key.share.root_folder_id.clone();
            let metadata = DtoToMetadataConverter::get_fresh_node_metadata(
                client,
                root_folder_id,
                Some(share_and_key),
            )
            .await?;
            return metadata.get_folder_node_or_throw();
        }

        let share_response = client.api().shares().get_my_files_share().await?;
        let (volume_dto, share_dto, link_details) = share_response.deconstruct();

        let (share, share_key) = crate::share_ops::ShareCrypto::decrypt_share(
            client,
            share_dto.id.clone(),
            &share_dto.key,
            &share_dto.passphrase,
            &share_dto.passphrase_signature,
            share_dto
                .invitee_share_passphrase_session_key_signature
                .as_ref(),
            &share_dto.creator_email_address,
            &share_dto.address_id,
        )
        .await?;

        client
            .cache()
            .entities()
            .set_main_volume_id(volume_dto.id.clone())
            .await?;
        client
            .cache()
            .entities()
            .set_my_files_share_id(share_dto.id.clone())
            .await?;
        client.cache().entities().set_share(share.clone()).await?;
        client
            .cache()
            .secrets()
            .set_share_key(share_dto.id.clone(), share_key.clone())
            .await?;

        let metadata_result = DtoToMetadataConverter::convert_dto_to_node_metadata(
            client.account().clone(),
            client.cache().entities().as_ref(),
            client.cache().secrets().as_ref(),
            volume_dto.id.clone(),
            link_details,
            Some(&share_key),
        )
        .await?;

        // Cache the root folder secrets
        if let PotentialObject::Node(metadata) = &metadata_result {
            if let NodeAndSecrets::Folder(_, secrets) = &metadata.inner {
                client
                    .cache()
                    .secrets()
                    .set_folder_secrets(
                        metadata.node().uid().clone(),
                        PotentialObject::Node(secrets.clone()),
                    )
                    .await?;
            }
        }

        metadata_result.get_folder_node_or_throw()
    }

    pub async fn get_node(
        client: &ProtonDriveClient,
        uid: NodeUid,
    ) -> anyhow::Result<PotentialObject<Node, DegradedNode>> {
        let metadata = Self::get_node_metadata(client, uid).await?;
        Ok(metadata.to_node_result())
    }

    pub async fn enumerate_nodes(
        client: &ProtonDriveClient,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        let mut results = Vec::with_capacity(uids.len());
        for uid in uids {
            match Self::get_node(client, uid).await {
                Ok(node) => results.push(node),
                Err(e) => {
                    return Err(e);
                }
            }
        }
        Ok(results)
    }

    pub async fn get_available_name(
        client: &ProtonDriveClient,
        parent_uid: NodeUid,
        name: String,
    ) -> anyhow::Result<String> {
        let folder_secrets =
            crate::node::folder::FolderOperations::get_secrets(client, parent_uid.clone()).await?;

        let mut candidate_names = vec![name.clone()];
        for i in 1..20 {
            if let Some(pos) = name.rfind('.') {
                let (base, ext) = name.split_at(pos);
                candidate_names.push(format!("{} ({}){}", base, i, ext));
            } else {
                candidate_names.push(format!("{} ({})", name, i));
            }
        }

        let mut hashes = Vec::new();
        for candidate in &candidate_names {
            let mut hmac = HmacSha256::new_from_slice(&folder_secrets.hash_key)?;
            hmac.update(candidate.as_bytes());
            hashes.push(hex::encode(hmac.finalize().into_bytes()));
        }

        let response = client
            .api()
            .links()
            .get_available_names(
                parent_uid.volume_id.clone(),
                parent_uid.link_id.clone(),
                crate::api::node::NodeNameAvailabilityRequest {
                    name_hash_digests: hashes,
                    client_uid: vec![],
                },
            )
            .await?;

        if let Some(_available_hash) = response.available_name_hash_digests.first() {
            let index = 0; // simplified
            return Ok(candidate_names[index].clone());
        }

        anyhow::bail!("No available names found")
    }

    pub async fn move_single(
        client: &ProtonDriveClient,
        uid: NodeUid,
        new_parent_uid: NodeUid,
        new_name: Option<String>,
    ) -> anyhow::Result<()> {
        let membership_address = Self::get_membership_address(client, &new_parent_uid).await?;
        let signing_key = client
            .account()
            .get_address_primary_private_key(&crate::account::AddressId::new(
                membership_address.address_id.clone(),
            ))
            .await?;

        let destination_folder_secrets =
            crate::node::folder::FolderOperations::get_secrets(client, new_parent_uid.clone())
                .await?;

        if uid == new_parent_uid {
            anyhow::bail!("Node {} cannot be moved onto itself", uid);
        }

        if uid.volume_id != new_parent_uid.volume_id {
            anyhow::bail!(
                "Node {} cannot have destination node {} as parent as they are not on the same volume",
                uid,
                new_parent_uid
            );
        }

        let metadata_result = Self::get_node_metadata(client, uid.clone()).await?;
        let (node, node_and_secrets, _, origin_name_hash_digest) =
            metadata_result.result()?.deconstruct();
        let secrets = match node_and_secrets {
            crate::node::NodeAndSecrets::File(_, s) => s.base,
            crate::node::NodeAndSecrets::Folder(_, s) => s.base,
        };

        let name_to_use = new_name.unwrap_or_else(|| match &node {
            Node::Folder(f) => f.base.name.clone(),
            Node::File(f) => f.base.base.name.clone(),
            Node::Photo(f) => f.base.base.name.clone(),
            Node::Album(f) => f.base.name.clone(),
        });

        let name_session_key = crate::crypto::CryptoGenerator::generate_session_key();
        let encrypted_name = crate::node::crypto::NodeCrypto::encrypt_name(
            &name_to_use,
            &name_session_key,
            &destination_folder_secrets.base.key,
            &PgpPrivateKey(signing_key.clone()),
        )?;

        let mut hmac = HmacSha256::new_from_slice(&destination_folder_secrets.hash_key)?;
        hmac.update(name_to_use.as_bytes());
        let name_hash_digest = hmac.finalize().into_bytes().to_vec();

        let passphrase_bytes = secrets
            .passphrase_for_anonymous_move
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Passphrase for anonymous move missing"))?;

        let (encrypted_passphrase, passphrase_signature, _) =
            crate::node::crypto::NodeCrypto::encrypt_and_sign_passphrase(
                passphrase_bytes,
                &destination_folder_secrets.base.key,
                &PgpPrivateKey(signing_key.clone()),
            )?;

        let request = crate::api::links::MoveSingleLinkRequest {
            name: encrypted_name,
            passphrase: encrypted_passphrase,
            name_hash_digest: name_hash_digest.clone(),
            parent_link_id: new_parent_uid.link_id.clone(),
            original_name_hash_digest: origin_name_hash_digest,
            name_signature_email_address: membership_address.email_address.clone(),
            passphrase_signature,
            signature_email_address: Some(membership_address.email_address),
        };

        client
            .api()
            .links()
            .move_link(
                new_parent_uid.volume_id.clone(),
                uid.link_id.clone(),
                request,
            )
            .await?;

        Ok(())
    }

    pub async fn move_multiple(
        client: &ProtonDriveClient,
        uids: Vec<NodeUid>,
        new_parent_uid: NodeUid,
    ) -> anyhow::Result<()> {
        if uids.is_empty() {
            return Ok(());
        }

        let membership_address = Self::get_membership_address(client, &new_parent_uid).await?;
        let signing_key = client
            .account()
            .get_address_primary_private_key(&crate::account::AddressId::new(
                membership_address.address_id.clone(),
            ))
            .await?;

        let destination_folder_secrets =
            crate::node::folder::FolderOperations::get_secrets(client, new_parent_uid.clone())
                .await?;

        let mut batch = Vec::new();

        for uid in uids {
            if uid == new_parent_uid {
                anyhow::bail!("Node {} cannot be moved onto itself", uid);
            }

            if uid.volume_id != new_parent_uid.volume_id {
                anyhow::bail!(
                    "Node {} cannot have destination node {} as parent as they are not on the same volume",
                    uid,
                    new_parent_uid
                );
            }

            let metadata_result = Self::get_node_metadata(client, uid.clone()).await?;
            let (node, node_and_secrets, _, origin_name_hash_digest) =
                metadata_result.result()?.deconstruct();
            let secrets = match node_and_secrets {
                crate::node::NodeAndSecrets::File(_, s) => s.base,
                crate::node::NodeAndSecrets::Folder(_, s) => s.base,
            };

            let name = match &node {
                Node::Folder(f) => f.base.name.clone(),
                Node::File(f) | Node::Photo(f) => f.base.base.name.clone(),
                Node::Album(f) => f.base.name.clone(),
            };

            let name_session_key = crate::crypto::CryptoGenerator::generate_session_key();
            let encrypted_name = crate::node::crypto::NodeCrypto::encrypt_name(
                &name,
                &name_session_key,
                &destination_folder_secrets.base.key,
                &PgpPrivateKey(signing_key.clone()),
            )?;

            let mut hmac = HmacSha256::new_from_slice(&destination_folder_secrets.hash_key)?;
            hmac.update(name.as_bytes());
            let name_hash_digest = hmac.finalize().into_bytes().to_vec();

            let passphrase_bytes = secrets
                .passphrase_for_anonymous_move
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Passphrase for anonymous move missing"))?;

            let (encrypted_passphrase, passphrase_signature, _) =
                crate::node::crypto::NodeCrypto::encrypt_and_sign_passphrase(
                    passphrase_bytes,
                    &destination_folder_secrets.base.key,
                    &PgpPrivateKey(signing_key.clone()),
                )?;

            batch.push(crate::api::links::MoveMultipleLinksItem {
                link_id: uid.link_id.clone(),
                name: encrypted_name,
                passphrase: encrypted_passphrase,
                passphrase_signature,
                name_hash_digest,
                original_name_hash_digest: origin_name_hash_digest,
            });
        }

        let request = crate::api::links::MoveMultipleLinksRequest {
            parent_link_id: new_parent_uid.link_id.clone(),
            batch,
            name_signature_email_address: membership_address.email_address.clone(),
            signature_email_address: Some(membership_address.email_address),
        };

        client
            .api()
            .links()
            .move_multiple(new_parent_uid.volume_id.clone(), request)
            .await?;

        Ok(())
    }

    pub async fn rename(
        client: &ProtonDriveClient,
        uid: NodeUid,
        new_name: String,
        new_media_type: Option<String>,
    ) -> anyhow::Result<()> {
        let metadata_result = Self::get_node_metadata(client, uid.clone()).await?;
        let (node, node_and_secrets, _, original_name_hash_digest) =
            metadata_result.result()?.deconstruct();
        let secrets = match node_and_secrets {
            crate::node::NodeAndSecrets::File(_, s) => s.base,
            crate::node::NodeAndSecrets::Folder(_, s) => s.base,
        };

        let parent_uid = match &node {
            Node::Folder(f) | Node::Album(f) => f.base.parent_uid.as_ref(),
            Node::File(f) | Node::Photo(f) => f.base.base.parent_uid.as_ref(),
        }
        .ok_or_else(|| anyhow::anyhow!("Cannot rename root node"))?;

        let membership_address = Self::get_membership_address(client, &uid).await?;
        let signing_key = client
            .account()
            .get_address_primary_private_key(&crate::account::AddressId::new(
                membership_address.address_id.clone(),
            ))
            .await?;

        let parent_folder_secrets =
            crate::node::folder::FolderOperations::get_secrets(client, parent_uid.clone()).await?;

        let name_session_key = crate::crypto::CryptoGenerator::generate_session_key();
        let encrypted_name = crate::node::crypto::NodeCrypto::encrypt_name(
            &new_name,
            &name_session_key,
            &parent_folder_secrets.base.key,
            &PgpPrivateKey(signing_key.clone()),
        )?;

        let mut hmac = HmacSha256::new_from_slice(&parent_folder_secrets.hash_key)?;
        hmac.update(new_name.as_bytes());
        let name_hash_digest = hmac.finalize().into_bytes().to_vec();

        let request = crate::api::links::RenameLinkRequest {
            name: encrypted_name,
            name_hash_digest,
            name_signature_email_address: membership_address.email_address,
            media_type: new_media_type,
            original_name_hash_digest,
        };

        client
            .api()
            .links()
            .rename(uid.volume_id.clone(), uid.link_id.clone(), request)
            .await?;

        Ok(())
    }

    pub async fn trash(
        client: &ProtonDriveClient,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<HashMap<NodeUid, Result<(), anyhow::Error>>> {
        let mut results = HashMap::new();
        let mut volume_groups: HashMap<crate::volume::VolumeId, Vec<crate::links::LinkId>> =
            HashMap::new();
        for uid in uids {
            volume_groups
                .entry(uid.volume_id.clone())
                .or_default()
                .push(uid.link_id.clone());
        }

        for (volume_id, link_ids) in volume_groups {
            let request = crate::api::links::MultipleLinksNullaryRequest {
                link_ids: link_ids.clone(),
            };
            match client
                .api()
                .trash()
                .trash_multiple(volume_id.clone(), request)
                .await
            {
                Ok(resp) => {
                    for pair in resp.responses {
                        let uid = NodeUid::new(volume_id.clone(), pair.link_id);
                        if pair.response.is_success() {
                            results.insert(uid, Ok(()));
                        } else {
                            results.insert(
                                uid,
                                Err(anyhow::anyhow!(
                                    pair.response.error_message.unwrap_or_default()
                                )),
                            );
                        }
                    }
                }
                Err(e) => {
                    for link_id in link_ids {
                        results.insert(
                            NodeUid::new(volume_id.clone(), link_id),
                            Err(anyhow::anyhow!(e.to_string())),
                        );
                    }
                }
            }
        }
        Ok(results)
    }

    pub async fn delete(
        client: &ProtonDriveClient,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<HashMap<NodeUid, Result<(), anyhow::Error>>> {
        let mut results = HashMap::new();
        let mut volume_groups: HashMap<crate::volume::VolumeId, Vec<crate::links::LinkId>> =
            HashMap::new();
        for uid in uids {
            volume_groups
                .entry(uid.volume_id.clone())
                .or_default()
                .push(uid.link_id.clone());
        }

        for (volume_id, link_ids) in volume_groups {
            match client
                .api()
                .links()
                .delete_multiple(volume_id.clone(), link_ids.clone())
                .await
            {
                Ok(resp) => {
                    for pair in resp.responses {
                        let uid = NodeUid::new(volume_id.clone(), pair.link_id);
                        if pair.response.is_success() {
                            results.insert(uid, Ok(()));
                        } else {
                            results.insert(
                                uid,
                                Err(anyhow::anyhow!(
                                    pair.response.error_message.unwrap_or_default()
                                )),
                            );
                        }
                    }
                }
                Err(e) => {
                    for link_id in link_ids {
                        results.insert(
                            NodeUid::new(volume_id.clone(), link_id),
                            Err(anyhow::anyhow!(e.to_string())),
                        );
                    }
                }
            }
        }
        Ok(results)
    }

    pub async fn restore(
        client: &ProtonDriveClient,
        uids: Vec<NodeUid>,
    ) -> anyhow::Result<HashMap<NodeUid, Result<(), anyhow::Error>>> {
        let mut results = HashMap::new();
        let mut volume_groups: HashMap<crate::volume::VolumeId, Vec<crate::links::LinkId>> =
            HashMap::new();
        for uid in uids {
            volume_groups
                .entry(uid.volume_id.clone())
                .or_default()
                .push(uid.link_id.clone());
        }

        for (volume_id, link_ids) in volume_groups {
            let request = crate::api::links::MultipleLinksNullaryRequest {
                link_ids: link_ids.clone(),
            };
            match client
                .api()
                .trash()
                .restore_multiple(volume_id.clone(), request)
                .await
            {
                Ok(resp) => {
                    for pair in resp.responses {
                        let uid = NodeUid::new(volume_id.clone(), pair.link_id);
                        if pair.response.is_success() {
                            results.insert(uid, Ok(()));
                        } else {
                            results.insert(
                                uid,
                                Err(anyhow::anyhow!(
                                    pair.response.error_message.unwrap_or_default()
                                )),
                            );
                        }
                    }
                }
                Err(e) => {
                    for link_id in link_ids {
                        results.insert(
                            NodeUid::new(volume_id.clone(), link_id),
                            Err(anyhow::anyhow!(e.to_string())),
                        );
                    }
                }
            }
        }
        Ok(results)
    }

    pub async fn get_node_metadata(
        client: &ProtonDriveClient,
        uid: NodeUid,
    ) -> anyhow::Result<NodeMetadataResult> {
        if let Some(cached_info) = client.cache().entities().try_get_node(uid.clone()).await? {
            match cached_info.node_provision_result {
                PotentialObject::Node(node) => match &node {
                    Node::Folder(_) | Node::Album(_) => {
                        if let Some(secrets) = client
                            .cache()
                            .secrets()
                            .try_get_folder_secrets(uid.clone())
                            .await?
                        {
                            let membership_share_id = cached_info.membership_share_id.clone();
                            let name_hash_digest = cached_info.name_hash_digest.clone();
                            let node_clone = node.clone();
                            let msid_clone = membership_share_id.clone();
                            let nhd_clone = name_hash_digest.clone();
                            return Ok(secrets.map_both(
                                move |s| NodeMetadata {
                                    inner: crate::node::NodeAndSecrets::Folder(
                                        match node_clone {
                                            Node::Folder(f) | Node::Album(f) => f,
                                            _ => unreachable!(),
                                        },
                                        s,
                                    ),
                                    membership_share_id: msid_clone,
                                    name_hash_digest: nhd_clone,
                                },
                                move |ds| crate::node::DegradedNodeMetadata {
                                    inner: crate::node::DegradedNodeAndSecrets::Folder(
                                        Default::default(),
                                        ds,
                                    ),
                                    membership_share_id,
                                    name_hash_digest,
                                },
                            ));
                        }
                    }
                    Node::File(_) | Node::Photo(_) => {
                        if let Some(secrets) = client
                            .cache()
                            .secrets()
                            .try_get_file_secrets(uid.clone())
                            .await?
                        {
                            let membership_share_id = cached_info.membership_share_id.clone();
                            let name_hash_digest = cached_info.name_hash_digest.clone();
                            let node_clone = node.clone();
                            let msid_clone = membership_share_id.clone();
                            let nhd_clone = name_hash_digest.clone();
                            return Ok(secrets.map_both(
                                move |s| NodeMetadata {
                                    inner: crate::node::NodeAndSecrets::File(
                                        match node_clone {
                                            Node::File(f) | Node::Photo(f) => f,
                                            _ => unreachable!(),
                                        },
                                        s,
                                    ),
                                    membership_share_id: msid_clone,
                                    name_hash_digest: nhd_clone,
                                },
                                move |ds| crate::node::DegradedNodeMetadata {
                                    inner: crate::node::DegradedNodeAndSecrets::File(
                                        Default::default(),
                                        ds,
                                    ),
                                    membership_share_id,
                                    name_hash_digest,
                                },
                            ));
                        }
                    }
                },
                PotentialObject::Degraded(_degraded_node) => {
                    // Similar logic for degraded if needed, but for now fallback to fresh
                }
            }
        }

        DtoToMetadataConverter::get_fresh_node_metadata(client, uid, None).await
    }

    async fn get_membership_address(
        client: &ProtonDriveClient,
        uid: &NodeUid,
    ) -> anyhow::Result<Address> {
        let response = client
            .api()
            .links()
            .get_context_share(uid.volume_id.clone(), uid.link_id.clone())
            .await?;

        let share = client
            .api()
            .shares()
            .get_share(response.context_share_id)
            .await?;

        let default_address = client.account().get_default_address().await?;
        for membership in &share.memberships {
            if membership.address_id.raw() == default_address.address_id {
                return Ok(default_address);
            }
        }

        Ok(default_address)
    }
}
