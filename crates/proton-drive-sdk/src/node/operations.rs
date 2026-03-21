use crate::client::ProtonDriveClient;
use crate::node::{
    DegradedNode, DtoToMetadataConverter, Node, NodeAndSecrets, NodeMetadata, NodeMetadataResult,
    NodeUid,
};
use crate::pgp::PgpPrivateKey;
use crate::share_ops::ShareOperations;
use crate::utils::PotentialObject;
use hmac::{Hmac, KeyInit, Mac};
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

    pub async fn move_multiple(
        client: &ProtonDriveClient,
        uids: Vec<NodeUid>,
        new_parent_uid: NodeUid,
    ) -> anyhow::Result<()> {
        if uids.is_empty() {
            return Ok(());
        }

        if uids.len() == 1 {
            return Self::move_single(client, uids[0].clone(), new_parent_uid, None).await;
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

            let metadata_result =
                crate::node::DtoToMetadataConverter::get_fresh_node_metadata(client, uid.clone(), None)
                    .await?;
            let (node, node_and_secrets, _, origin_name_hash_digest) =
                metadata_result.clone().result()?.deconstruct();
            let secrets = match node_and_secrets {
                crate::node::NodeAndSecrets::File(_, s) => s.base,
                crate::node::NodeAndSecrets::Folder(_, s) => s.base,
            };

            let name = match &node {
                Node::Folder(f) => f.base.name.clone(),
                Node::File(f) | Node::Photo(f) => f.base.base.name.clone(),
                Node::Album(f) => f.base.name.clone(),
            };

            let encrypted_name = crate::node::crypto::NodeCrypto::encrypt_name(
                &name,
                &secrets.name_session_key,
                &destination_folder_secrets.base.key,
                &PgpPrivateKey(signing_key.clone()),
            )?;

            let mut hmac = HmacSha256::new_from_slice(&destination_folder_secrets.hash_key)?;
            hmac.update(name.as_bytes());
            let name_hash_digest = hmac.finalize().into_bytes().to_vec();

            let is_anonymous = secrets.passphrase_for_anonymous_move.is_some();
            let (encrypted_passphrase, passphrase_signature) = if is_anonymous {
                let (passphrase, sig, _) = crate::node::crypto::NodeCrypto::encrypt_and_sign_passphrase(
                    &secrets.passphrase_session_key.key,
                    &destination_folder_secrets.base.key,
                    &PgpPrivateKey(signing_key.clone()),
                )?;
                (passphrase, sig)
            } else {
                let passphrase = crate::node::crypto::NodeCrypto::reencrypt_passphrase(
                    &secrets.passphrase_session_key.key,
                    secrets.passphrase_pgp_session_key.as_ref(),
                    &destination_folder_secrets.base.key,
                    &PgpPrivateKey(signing_key.clone()),
                )?;
                (passphrase, None)
            };

            let _media_type = match &node {
                Node::File(f) | Node::Photo(f) => Some(f.base.media_type.clone()),
                _ => None,
            };

            batch.push(crate::api::links::MoveMultipleLinksItem {
                link_id: uid.link_id.clone(),
                name: encrypted_name,
                passphrase: encrypted_passphrase,
                passphrase_signature,
                name_hash_digest,
                original_name_hash_digest: origin_name_hash_digest,
            });
        }

        let signature_email_address = if batch.iter().any(|i| i.passphrase_signature.is_some()) {
            Some(membership_address.email_address.clone())
        } else {
            None
        };

        let request = crate::api::links::MoveMultipleLinksRequest {
            parent_link_id: new_parent_uid.link_id.clone(),
            batch,
            name_signature_email_address: membership_address.email_address.clone(),
            signature_email_address,
        };

        client
            .api()
            .links()
            .move_multiple(new_parent_uid.volume_id.clone(), request.clone())
            .await?;

        // Update cache for each moved node
        for item in request.batch {
            let uid = NodeUid::new(new_parent_uid.volume_id.clone(), item.link_id);
            if let Some(mut cached_info) = client.cache().entities().try_get_node(uid.clone()).await? {
                if let PotentialObject::Node(ref mut node) = cached_info.node_provision_result {
                    node.set_parent_uid(Some(new_parent_uid.clone()));
                    // Note: name might have changed if we supported new_name in move_multiple,
                    // but currently we don't.
                }
                client.cache().entities().set_node(
                    uid,
                    cached_info.node_provision_result,
                    cached_info.membership_share_id,
                    item.name_hash_digest,
                ).await?;
            }
        }

        Ok(())
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

        let metadata_result =
            crate::node::DtoToMetadataConverter::get_fresh_node_metadata(client, uid.clone(), None)
                .await?;
        let (node, node_and_secrets, _membership_share_id, origin_name_hash_digest) =
            metadata_result.clone().result()?.deconstruct();
        let secrets = match node_and_secrets {
            crate::node::NodeAndSecrets::File(_, s) => s.base,
            crate::node::NodeAndSecrets::Folder(_, s) => s.base,
        };

        let name_to_use = new_name.as_ref().cloned().unwrap_or_else(|| match &node {
            Node::Folder(f) => f.base.name.clone(),
            Node::File(f) | Node::Photo(f) => f.base.base.name.clone(),
            Node::Album(f) => f.base.name.clone(),
        });

        log::debug!("Moving node: name={}, original_hash={}", name_to_use, hex::encode(&origin_name_hash_digest));

        let encrypted_name = crate::node::crypto::NodeCrypto::encrypt_name(
            &name_to_use,
            &secrets.name_session_key,
            &destination_folder_secrets.base.key,
            &PgpPrivateKey(signing_key.clone()),
        )?;

        let mut hmac = HmacSha256::new_from_slice(&destination_folder_secrets.hash_key)?;
        hmac.update(name_to_use.as_bytes());
        let name_hash_digest = hmac.finalize().into_bytes().to_vec();

        let is_anonymous = secrets.passphrase_for_anonymous_move.is_some();
        let (encrypted_passphrase, passphrase_signature, signature_email_address) = if is_anonymous {
            let (passphrase, sig, _) = crate::node::crypto::NodeCrypto::encrypt_and_sign_passphrase(
                &secrets.passphrase_session_key.key,
                &destination_folder_secrets.base.key,
                &PgpPrivateKey(signing_key.clone()),
            )?;
            (passphrase, sig, Some(membership_address.email_address.clone()))
        } else {
            let passphrase = crate::node::crypto::NodeCrypto::reencrypt_passphrase(
                &secrets.passphrase_session_key.key,
                secrets.passphrase_pgp_session_key.as_ref(),
                &destination_folder_secrets.base.key,
                &PgpPrivateKey(signing_key.clone()),
            )?;
            (passphrase, None, None)
        };

        let request = crate::api::links::MoveSingleLinkRequest {
            name: encrypted_name,
            passphrase: encrypted_passphrase,
            name_hash_digest: name_hash_digest.clone(),
            parent_link_id: new_parent_uid.link_id.clone(),
            original_name_hash_digest: origin_name_hash_digest,
            name_signature_email_address: membership_address.email_address.clone(),
            content_hash: None,
            passphrase_signature,
            signature_email_address,
        };

        client
            .api()
            .links()
            .move_link(
                uid.volume_id.clone(),
                uid.link_id.clone(),
                request,
            )
            .await?;

        // Update cache
        let (mut node, _, membership_share_id, _) = metadata_result.clone().result()?.deconstruct();

        node.set_parent_uid(Some(new_parent_uid));
        if let Some(name) = new_name {
            node.set_name(name);
        }

        client.cache().entities().set_node(
            uid,
            PotentialObject::Node(node),
            membership_share_id,
            name_hash_digest,
        ).await?;

        Ok(())
    }

    /// Server-side copy: re-encrypts the node's key material under the
    /// destination parent's key and calls the copy endpoint. No file data
    /// is transferred — the server creates a new link pointing to the same
    /// revision.
    pub async fn copy_single(
        client: &ProtonDriveClient,
        uid: NodeUid,
        new_parent_uid: NodeUid,
        new_name: Option<String>,
    ) -> anyhow::Result<crate::links::LinkId> {
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

        let metadata_result =
            crate::node::DtoToMetadataConverter::get_fresh_node_metadata(client, uid.clone(), None)
                .await?;
        let (node, node_and_secrets, _membership_share_id, _origin_name_hash_digest) =
            metadata_result.result()?.deconstruct();
        let secrets = match node_and_secrets {
            crate::node::NodeAndSecrets::File(_, s) => s.base,
            crate::node::NodeAndSecrets::Folder(_, s) => s.base,
        };

        let name_to_use = new_name.unwrap_or_else(|| match &node {
            Node::Folder(f) => f.base.name.clone(),
            Node::File(f) | Node::Photo(f) => f.base.base.name.clone(),
            Node::Album(f) => f.base.name.clone(),
        });

        let encrypted_name = crate::node::crypto::NodeCrypto::encrypt_name(
            &name_to_use,
            &secrets.name_session_key,
            &destination_folder_secrets.base.key,
            &PgpPrivateKey(signing_key.clone()),
        )?;

        let mut hmac = HmacSha256::new_from_slice(&destination_folder_secrets.hash_key)?;
        Mac::update(&mut hmac, name_to_use.as_bytes());
        let name_hash_digest = Mac::finalize(hmac).into_bytes().to_vec();

        let is_anonymous = secrets.passphrase_for_anonymous_move.is_some();
        let (encrypted_passphrase, passphrase_signature, signature_email_address) = if is_anonymous {
            let (passphrase, sig, _) = crate::node::crypto::NodeCrypto::encrypt_and_sign_passphrase(
                &secrets.passphrase_session_key.key,
                &destination_folder_secrets.base.key,
                &PgpPrivateKey(signing_key.clone()),
            )?;
            (passphrase, sig, Some(membership_address.email_address.clone()))
        } else {
            let passphrase = crate::node::crypto::NodeCrypto::reencrypt_passphrase(
                &secrets.passphrase_session_key.key,
                secrets.passphrase_pgp_session_key.as_ref(),
                &destination_folder_secrets.base.key,
                &PgpPrivateKey(signing_key.clone()),
            )?;
            (passphrase, None, None)
        };

        let request = crate::api::links::CopyLinkRequest {
            target_volume_id: new_parent_uid.volume_id.clone(),
            target_parent_link_id: new_parent_uid.link_id.clone(),
            name: encrypted_name,
            passphrase: encrypted_passphrase,
            name_hash_digest,
            name_signature_email_address: membership_address.email_address.clone(),
            passphrase_signature,
            signature_email_address,
        };

        let response = client
            .api()
            .links()
            .copy_link(
                uid.volume_id.clone(),
                uid.link_id.clone(),
                request,
            )
            .await?;

        Ok(response.link_id)
    }

    pub async fn rename(
        client: &ProtonDriveClient,
        uid: NodeUid,
        new_name: String,
        new_media_type: Option<String>,
    ) -> anyhow::Result<()> {
        let metadata_result =
            crate::node::DtoToMetadataConverter::get_fresh_node_metadata(client, uid.clone(), None)
                .await?;
        let (node, node_and_secrets, _, original_name_hash_digest) =
            metadata_result.clone().result()?.deconstruct();
        let _secrets = match node_and_secrets {
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
            name_hash_digest: name_hash_digest.clone(),
            name_signature_email_address: membership_address.email_address,
            media_type: new_media_type,
            original_name_hash_digest,
        };

        client
            .api()
            .links()
            .rename(uid.volume_id.clone(), uid.link_id.clone(), request)
            .await?;

        // Update cache
        if let Some(mut cached_info) = client.cache().entities().try_get_node(uid.clone()).await? {
            if let PotentialObject::Node(ref mut node) = cached_info.node_provision_result {
                node.set_name(new_name);
            }
            client.cache().entities().set_node(
                uid,
                cached_info.node_provision_result,
                cached_info.membership_share_id,
                name_hash_digest,
            ).await?;
        }

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

    pub async fn delete_from_trash(
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
                .delete_multiple(volume_id.clone(), request)
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
        let mut current_uid = uid.clone();
        let mut visited = std::collections::HashSet::new();

        loop {
            if !visited.insert(current_uid.clone()) {
                anyhow::bail!("Folder structure loop detected");
            }

            let metadata = Self::get_node_metadata(client, current_uid.clone()).await?;
            let result = metadata.result().map_err(|e| anyhow::anyhow!(e.to_string()))?;

            if let Some(share_id) = &result.membership_share_id {
                let share_and_key = ShareOperations::get_share(client, share_id.clone()).await?;
                let membership_address_id = share_and_key.share.membership_address_id.clone();
                return client.account().get_address(&membership_address_id).await;
            }

            match result.inner.parent_uid() {
                Some(parent_uid) => {
                    current_uid = parent_uid.clone();
                }
                None => {
                    // Fallback to API if we reached the root and still no share_id
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

                    return client.account().get_address(&share.address_id).await;
                }
            }
        }
    }
}
