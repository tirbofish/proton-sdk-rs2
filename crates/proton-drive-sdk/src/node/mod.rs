use std::fmt::Display;

use crate::author::Author;
use crate::client::ProtonDriveClient;
use crate::error::ProtonDriveError;
use crate::links::LinkId;
use crate::node::file::{
    DegradedFileMetadata, DegradedFileNode, DegradedFileSecrets, FileMetadata, FileNode,
    FileOrFileDraftNode, FileSecrets,
};
use crate::node::folder::{
    DegradedFolderMetadata, DegradedFolderNode, DegradedFolderSecrets, FolderMetadata, FolderNode,
    FolderSecrets,
};
use crate::node::secrets::{DegradedNodeSecrets, ShareAndKey};
use crate::pgp::{PgpPrivateKey, PgpSessionKey};
use crate::protobuf::SignatureVerificationError;
use crate::share::ShareId;
use crate::utils::PotentialObject;
use crate::volume::VolumeId;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod authorship;
pub mod crypto;
pub mod download;
pub mod file;
pub mod folder;
pub mod photo;
pub mod revision;
pub mod secrets;
pub mod thumbnail;
pub mod transfer;

pub mod draft;
pub mod operations;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DegradedNode {
    Folder(DegradedFolderNode),
    File(DegradedFileNode),
    Photo(DegradedFileNode),
    Album(DegradedFolderNode),
}

impl DegradedNode {
    pub fn uid(&self) -> &NodeUid {
        match self {
            DegradedNode::Folder(n) | DegradedNode::Album(n) => &n.base.uid,
            DegradedNode::File(n) | DegradedNode::Photo(n) => &n.base.uid,
        }
    }

    pub fn parent_uid(&self) -> Option<&NodeUid> {
        match self {
            Self::Folder(n) | Self::Album(n) => n.base.parent_uid.as_ref(),
            Self::File(n) | Self::Photo(n) => n.base.parent_uid.as_ref(),
        }
    }

    pub fn tree_event_scope_id(&self) -> String {
        self.uid().volume_id.raw().to_string()
    }

    pub fn set_parent_uid(&mut self, parent_uid: Option<NodeUid>) {
        match self {
            Self::Folder(n) | Self::Album(n) => n.base.parent_uid = parent_uid,
            Self::File(n) | Self::Photo(n) => n.base.parent_uid = parent_uid,
        }
    }

    pub fn set_name(&mut self, name: String) {
        match self {
            Self::Folder(n) | Self::Album(n) => n.base.name = PotentialObject::Node(name),
            Self::File(n) | Self::Photo(n) => n.base.name = PotentialObject::Node(name),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DegradedNodeBase {
    pub uid: NodeUid,
    pub parent_uid: Option<NodeUid>,
    pub name: PotentialObject<String, ProtonDriveError>,
    pub name_author: PotentialObject<Author, SignatureVerificationError>,
    pub creation_time: DateTime<Utc>,
    pub trash_time: Option<DateTime<Utc>>,
    pub author: PotentialObject<Author, SignatureVerificationError>,
    pub owned_by: OwnedBy,
    pub errors: Vec<ProtonDriveError>,
}

#[derive(Debug, Clone)]
pub enum DegradedNodeAndSecrets {
    File(DegradedFileNode, DegradedFileSecrets),
    Folder(DegradedFolderNode, DegradedFolderSecrets),
}

impl DegradedNodeAndSecrets {
    pub fn try_get_file_else_folder(
        self,
    ) -> Result<(DegradedFileNode, DegradedFileSecrets), (DegradedFolderNode, DegradedFolderSecrets)>
    {
        match self {
            Self::File(n, s) => Ok((n, s)),
            Self::Folder(n, s) => Err((n, s)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DegradedNodeMetadata {
    pub inner: DegradedNodeAndSecrets,
    pub membership_share_id: Option<ShareId>,
    pub name_hash_digest: Vec<u8>,
}

impl DegradedNodeMetadata {
    pub fn from_file(m: DegradedFileMetadata) -> Self {
        Self {
            inner: DegradedNodeAndSecrets::File(m.node, m.secrets),
            membership_share_id: m.membership_share_id,
            name_hash_digest: m.name_hash_digest,
        }
    }

    pub fn from_folder(m: DegradedFolderMetadata) -> Self {
        Self {
            inner: DegradedNodeAndSecrets::Folder(m.node, m.secrets),
            membership_share_id: m.membership_share_id,
            name_hash_digest: m.name_hash_digest,
        }
    }

    pub fn node(&self) -> DegradedNode {
        match &self.inner {
            DegradedNodeAndSecrets::File(n, _) => DegradedNode::File(n.clone()),
            DegradedNodeAndSecrets::Folder(n, _) => DegradedNode::Folder(n.clone()),
        }
    }

    pub fn deconstruct(
        self,
    ) -> (
        DegradedNode,
        DegradedNodeAndSecrets,
        Option<ShareId>,
        Vec<u8>,
    ) {
        let node = self.node();
        (
            node,
            self.inner,
            self.membership_share_id,
            self.name_hash_digest,
        )
    }

    pub fn try_get_file_else_folder(
        self,
    ) -> Result<(DegradedFileNode, DegradedFileSecrets), (DegradedFolderNode, DegradedFolderSecrets)>
    {
        self.inner.try_get_file_else_folder()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(try_from = "String", into = "String")]
pub struct NodeUid {
    pub volume_id: VolumeId,
    pub link_id: LinkId,
}

impl NodeUid {
    pub fn new(volume_id: VolumeId, link_id: LinkId) -> Self {
        Self { volume_id, link_id }
    }

    pub fn try_parse(s: &str) -> Option<Self> {
        let (volume, link) = s.split_once('~')?;
        Some(Self {
            volume_id: VolumeId::new(volume.to_string()),
            link_id: LinkId::new(link.to_string()),
        })
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        Self::try_parse(s).ok_or_else(|| format!("Invalid node UID format: \"{}\"", s))
    }

    pub fn raw(&self) -> String {
        format!("{}~{}", self.volume_id.raw(), self.link_id.raw())
    }
}

impl std::fmt::Display for NodeUid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}~{}", self.volume_id.raw(), self.link_id.raw())
    }
}

impl From<NodeUid> for String {
    fn from(uid: NodeUid) -> Self {
        uid.to_string()
    }
}

impl TryFrom<String> for NodeUid {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        NodeUid::parse(&s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeType {
    Folder,
    File,
    Photo,
    Album,
}

impl Display for NodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Node {
    Folder(FolderNode),
    File(FileNode),
    Photo(FileNode),
    Album(FolderNode),
}

impl Node {
    pub fn uid(&self) -> &NodeUid {
        match self {
            Node::Folder(n) | Node::Album(n) => &n.base.uid,
            Node::File(n) | Node::Photo(n) => &n.base.base.uid,
        }
    }

    pub fn base(&self) -> &NodeBase {
        match self {
            Node::Folder(n) | Node::Album(n) => &n.base,
            Node::File(n) | Node::Photo(n) => &n.base.base,
        }
    }

    /// Returns the type of the current Node, in the case you do not want to match it.
    pub fn ty(&self) -> NodeType {
        match self {
            Node::Folder(_) => NodeType::Folder,
            Node::File(_) => NodeType::File,
            Node::Photo(_) => NodeType::Photo,
            Node::Album(_) => NodeType::Album,
        }
    }

    pub fn set_parent_uid(&mut self, parent_uid: Option<NodeUid>) {
        match self {
            Node::Folder(n) | Node::Album(n) => n.base.parent_uid = parent_uid,
            Node::File(n) | Node::Photo(n) => n.base.base.parent_uid = parent_uid,
        }
    }

    pub fn set_name(&mut self, name: String) {
        match self {
            Node::Folder(n) | Node::Album(n) => n.base.name = name,
            Node::File(n) | Node::Photo(n) => n.base.base.name = name,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeBase {
    pub uid: NodeUid,
    pub parent_uid: Option<NodeUid>,
    pub name: String,
    pub creation_time: DateTime<Utc>,
    pub trash_time: Option<DateTime<Utc>>,
    pub name_author: PotentialObject<Author, SignatureVerificationError>,
    pub author: PotentialObject<Author, SignatureVerificationError>,
    pub owned_by: Option<OwnedBy>,
}

impl NodeBase {
    pub fn tree_event_scope_id(&self) -> String {
        self.uid.volume_id.raw().to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSecrets {
    pub key: PgpPrivateKey,
    pub passphrase_session_key: PgpSessionKey,
    pub name_session_key: PgpSessionKey,
    #[serde(rename = "passphrase")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase_for_anonymous_move: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub enum NodeAndSecrets {
    File(FileNode, FileSecrets),
    Folder(FolderNode, FolderSecrets),
}

impl NodeAndSecrets {
    pub fn try_get_file_else_folder(
        self,
    ) -> Result<(FileNode, FileSecrets), (FolderNode, FolderSecrets)> {
        match self {
            Self::File(n, s) => Ok((n, s)),
            Self::Folder(n, s) => Err((n, s)),
        }
    }

    pub fn parent_uid(&self) -> Option<&NodeUid> {
        match self {
            Self::File(n, _) => n.base.base.parent_uid.as_ref(),
            Self::Folder(n, _) => n.base.parent_uid.as_ref(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NodeMetadata {
    pub inner: NodeAndSecrets,
    pub membership_share_id: Option<ShareId>,
    pub name_hash_digest: Vec<u8>,
}

impl NodeMetadata {
    pub fn from_file(m: FileMetadata) -> Self {
        Self {
            inner: NodeAndSecrets::File(m.node, m.secrets),
            membership_share_id: m.membership_share_id,
            name_hash_digest: m.name_hash_digest,
        }
    }

    pub fn from_folder(m: FolderMetadata) -> Self {
        Self {
            inner: NodeAndSecrets::Folder(m.node, m.secrets),
            membership_share_id: m.membership_share_id,
            name_hash_digest: m.name_hash_digest,
        }
    }

    pub fn node(&self) -> Node {
        match &self.inner {
            NodeAndSecrets::File(n, _) => Node::File(n.clone()),
            NodeAndSecrets::Folder(n, _) => Node::Folder(n.clone()),
        }
    }

    pub fn try_get_file_else_folder(
        self,
    ) -> Result<(FileNode, FileSecrets), (FolderNode, FolderSecrets)> {
        self.inner.try_get_file_else_folder()
    }

    pub fn deconstruct(self) -> (Node, NodeAndSecrets, Option<ShareId>, Vec<u8>) {
        let node = self.node();
        (
            node,
            self.inner,
            self.membership_share_id,
            self.name_hash_digest,
        )
    }
}

pub type NodeMetadataResult = PotentialObject<NodeMetadata, DegradedNodeMetadata>;

impl NodeMetadataResult {
    pub fn get_node_or_throw(self) -> anyhow::Result<Node> {
        let metadata = self.result()?;
        Ok(metadata.node())
    }

    pub fn get_folder_node_or_throw(self) -> anyhow::Result<FolderNode> {
        let metadata = self.result()?;
        match metadata.try_get_file_else_folder() {
            Ok((file, _)) => anyhow::bail!("Expected folder, got file: {}", file.base.base.uid),
            Err((folder, _)) => Ok(folder),
        }
    }

    pub fn get_folder_secrets_or_throw(self) -> anyhow::Result<FolderSecrets> {
        let metadata = self.result()?;
        match metadata.try_get_file_else_folder() {
            Ok((file, _)) => anyhow::bail!("Expected folder, got file: {}", file.base.base.uid),
            Err((_, secrets)) => Ok(secrets),
        }
    }

    pub fn try_get_folder_secrets_else_error(self) -> anyhow::Result<FolderSecrets> {
        match self {
            PotentialObject::Node(m) => match m.inner {
                crate::node::NodeAndSecrets::File(_, _) => {
                    anyhow::bail!("Node is a file, not a folder")
                }
                crate::node::NodeAndSecrets::Folder(_, s) => Ok(s),
            },
            PotentialObject::Degraded(_) => {
                anyhow::bail!("Degraded node has no folder secrets")
            }
        }
    }

    pub fn to_secrets_result(
        self,
    ) -> PotentialObject<NodeSecrets, crate::node::secrets::DegradedNodeSecrets> {
        match self {
            PotentialObject::Node(m) => {
                let secrets = match m.inner {
                    NodeAndSecrets::File(_, s) => s.base,
                    NodeAndSecrets::Folder(_, s) => s.base,
                };
                PotentialObject::Node(secrets)
            }
            PotentialObject::Degraded(m) => {
                let secrets = match m.inner {
                    crate::node::DegradedNodeAndSecrets::File(_, s) => s.base,
                    crate::node::DegradedNodeAndSecrets::Folder(_, s) => s.base,
                };
                PotentialObject::Degraded(secrets)
            }
        }
    }

    pub fn to_node_result(self) -> PotentialObject<Node, DegradedNode> {
        match self {
            PotentialObject::Node(m) => PotentialObject::Node(m.node()),
            PotentialObject::Degraded(m) => PotentialObject::Degraded(m.node()),
        }
    }
}

pub struct NodeBatchLoader {
    inner: crate::utils::batch::BatchLoaderBase<
        crate::links::LinkId,
        PotentialObject<Node, DegradedNode>,
        NodeBatchLoaderImpl,
    >,
}

struct NodeBatchLoaderImpl {
    client: std::sync::Arc<ProtonDriveClient>,
    volume_id: VolumeId,
    parent_key: Option<crate::pgp::PgpPrivateKey>,
}

#[async_trait]
impl crate::utils::batch::BatchLoader<crate::links::LinkId, PotentialObject<Node, DegradedNode>>
    for NodeBatchLoaderImpl
{
    async fn load_batch(
        &self,
        ids: Vec<crate::links::LinkId>,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        let response = self
            .client
            .api()
            .links()
            .get_details(self.volume_id.clone(), ids)
            .await?;
        let mut results = Vec::with_capacity(response.links.len());

        for link_details in response.links {
            let metadata_result = DtoToMetadataConverter::convert_dto_to_node_metadata(
                self.client.account().clone(),
                self.client.cache().entities().as_ref(),
                self.client.cache().secrets().as_ref(),
                self.volume_id.clone(),
                link_details,
                self.parent_key.as_ref(),
            )
            .await?;

            results.push(metadata_result.to_node_result());
        }

        Ok(results)
    }
}

impl NodeBatchLoader {
    pub fn new(
        client: std::sync::Arc<ProtonDriveClient>,
        volume_id: VolumeId,
        parent_key: Option<crate::pgp::PgpPrivateKey>,
    ) -> Self {
        Self {
            inner: crate::utils::batch::BatchLoaderBase::new(
                NodeBatchLoaderImpl {
                    client,
                    volume_id,
                    parent_key,
                },
                50,
            ),
        }
    }

    pub async fn queue_and_try_load_batch(
        &mut self,
        id: crate::links::LinkId,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        self.inner.queue_and_try_load_batch(id).await
    }

    pub async fn load_remaining(
        &mut self,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        self.inner.load_remaining().await
    }
}

pub struct VolumeTrashBatchLoader {
    inner: crate::utils::batch::BatchLoaderBase<
        crate::links::LinkId,
        PotentialObject<Node, DegradedNode>,
        VolumeTrashBatchLoaderImpl,
    >,
}

struct VolumeTrashBatchLoaderImpl {
    client: std::sync::Arc<ProtonDriveClient>,
    volume_id: VolumeId,
    share_key: crate::pgp::PgpPrivateKey,
    parent_keys: tokio::sync::Mutex<
        std::collections::HashMap<crate::links::LinkId, crate::pgp::PgpPrivateKey>,
    >,
}

#[async_trait]
impl crate::utils::batch::BatchLoader<crate::links::LinkId, PotentialObject<Node, DegradedNode>>
    for VolumeTrashBatchLoaderImpl
{
    async fn load_batch(
        &self,
        ids: Vec<crate::links::LinkId>,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        let response = self
            .client
            .api()
            .links()
            .get_details(self.volume_id.clone(), ids.clone())
            .await?;

        let mut results = Vec::with_capacity(ids.len());

        for link_details in response.links {
            let parent_key = if let Some(parent_id) = &link_details.link.parent_id {
                let mut parent_keys = self.parent_keys.lock().await;
                if let Some(key) = parent_keys.get(parent_id) {
                    key.clone()
                } else {
                    let folder_secrets = crate::node::folder::FolderOperations::get_secrets(
                        &self.client,
                        NodeUid::new(self.volume_id.clone(), parent_id.clone()),
                    )
                    .await?;
                    let key = folder_secrets.base.key;
                    parent_keys.insert(parent_id.clone(), key.clone());
                    key
                }
            } else {
                self.share_key.clone()
            };

            let metadata_result = DtoToMetadataConverter::convert_dto_to_node_metadata(
                self.client.account().clone(),
                self.client.cache().entities().as_ref(),
                self.client.cache().secrets().as_ref(),
                self.volume_id.clone(),
                link_details,
                Some(&parent_key),
            )
            .await?;

            results.push(metadata_result.to_node_result());
        }

        Ok(results)
    }
}

impl VolumeTrashBatchLoader {
    pub fn new(
        client: std::sync::Arc<ProtonDriveClient>,
        volume_id: VolumeId,
        share_key: crate::pgp::PgpPrivateKey,
    ) -> Self {
        Self {
            inner: crate::utils::batch::BatchLoaderBase::new(
                VolumeTrashBatchLoaderImpl {
                    client,
                    volume_id,
                    share_key,
                    parent_keys: tokio::sync::Mutex::new(std::collections::HashMap::new()),
                },
                50,
            ),
        }
    }

    pub async fn queue_and_try_load_batch(
        &mut self,
        id: crate::links::LinkId,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        self.inner.queue_and_try_load_batch(id).await
    }

    pub async fn load_remaining(
        &mut self,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        self.inner.load_remaining().await
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct OwnedBy {
    pub email: Option<String>,
    pub organisation: Option<String>,
}

pub struct DtoToMetadataConverter;

impl DtoToMetadataConverter {
    pub async fn get_fresh_node_metadata(
        client: &ProtonDriveClient,
        uid: crate::node::NodeUid,
        known_share_and_key: Option<ShareAndKey>,
    ) -> anyhow::Result<NodeMetadataResult> {
        let response = client
            .api()
            .links()
            .get_details(uid.volume_id.clone(), vec![uid.link_id.clone()])
            .await?;

        if response.links.is_empty() {
            anyhow::bail!("Node not found: {}", uid);
        }

        let metadata = Self::convert_dto_to_node_metadata(
            client.account().clone(),
            client.cache().entities().as_ref(),
            client.cache().secrets().as_ref(),
            uid.volume_id.clone(),
            response.links[0].clone(),
            known_share_and_key.map(|s| s.key).as_ref(),
        )
        .await?;

        // Cache the metadata and secrets
        if let PotentialObject::Node(m) = &metadata {
            client
                .cache()
                .entities()
                .set_node(
                    m.node().uid().clone(),
                    metadata.clone().to_node_result(),
                    m.membership_share_id.clone(),
                    m.name_hash_digest.clone(),
                )
                .await?;
            match &m.inner {
                NodeAndSecrets::Folder(_, secrets) => {
                    client
                        .cache()
                        .secrets()
                        .set_folder_secrets(
                            m.node().uid().clone(),
                            PotentialObject::Node(secrets.clone()),
                        )
                        .await?;
                }
                NodeAndSecrets::File(_, secrets) => {
                    client
                        .cache()
                        .secrets()
                        .set_file_secrets(
                            m.node().uid().clone(),
                            PotentialObject::Node(secrets.clone()),
                        )
                        .await?;
                }
            }
        }

        Ok(metadata)
    }

    pub async fn convert_dto_to_node_metadata(
        account_client: std::sync::Arc<dyn crate::account::AccountClient>,
        _entity_cache: &dyn crate::cache::entity::DriveEntityCache,
        _secret_cache: &dyn crate::cache::secret::DriveSecretCache,
        _volume_id: crate::volume::VolumeId,
        link_details: crate::api::links::LinkDetailsDto,
        parent_key: Option<&PgpPrivateKey>,
    ) -> anyhow::Result<NodeMetadataResult> {
        let link_dto = link_details.link.clone();
        let parent_key_result: Result<Vec<PgpPrivateKey>, String> = match parent_key {
            Some(k) => Ok(vec![k.clone()]),
            None => {
                // Try to find parent key from cache
                if let Some(parent_id) = &link_dto.parent_id {
                    tracing::debug!(parent_link_id = %parent_id.raw(), "Looking up parent folder key");
                    let parent_uid = NodeUid::new(_volume_id.clone(), parent_id.clone());
                    if let Some(secrets) = _secret_cache.try_get_folder_secrets(parent_uid).await? {
                        match secrets {
                            PotentialObject::Node(s) => {
                                tracing::debug!("Found parent folder key in cache");
                                Ok(vec![s.base.key])
                            }
                            PotentialObject::Degraded(_) => {
                                tracing::debug!("Parent folder key is degraded");
                                Err("Parent folder key is degraded".to_string())
                            }
                        }
                    } else {
                        tracing::debug!(
                            parent_link_id = %parent_id.raw(),
                            "Parent folder key NOT found in cache, trying user keys"
                        );
                        let user_keys = account_client.get_user_keys().await?;
                        if !user_keys.is_empty() {
                            tracing::debug!(
                                count = user_keys.len(),
                                "Found user keys to try as fallback"
                            );
                            Ok(user_keys.into_iter().map(PgpPrivateKey).collect())
                        } else {
                            Err(format!(
                                "Parent folder key for {:?} not found in cache and no user keys available",
                                parent_id
                            ))
                        }
                    }
                } else {
                    // Root folder - need share key
                    tracing::debug!("Looking up share key for root folder");
                    if let Some(sharing) = &link_details.sharing {
                        if let Some(share_key) = _secret_cache
                            .try_get_share_key(sharing.share_id.clone())
                            .await?
                        {
                            tracing::debug!(
                                share_id = %sharing.share_id.raw(),
                                "Found share key in cache"
                            );
                            Ok(vec![share_key])
                        } else {
                            tracing::debug!(
                                share_id = %sharing.share_id.raw(),
                                "Share key NOT found in cache"
                            );
                            Err(format!(
                                "Share key for {:?} not found in cache",
                                sharing.share_id
                            ))
                        }
                    } else {
                        // Fallback to My Files share?
                        if let Some(share_id) = _entity_cache.try_get_my_files_share_id().await? {
                            tracing::debug!(
                                share_id = %share_id.raw(),
                                "Falling back to My Files share key"
                            );
                            if let Some(share_key) =
                                _secret_cache.try_get_share_key(share_id).await?
                            {
                                Ok(vec![share_key])
                            } else {
                                Err("My Files share key not found in cache".to_string())
                            }
                        } else {
                            tracing::debug!(
                                "No parent key, no sharing, and no My Files fallback available"
                            );
                            Err("Parent key not provided and no sharing info available".to_string())
                        }
                    }
                }
            }
        };

        match link_dto.r#type {
            crate::api::links::LinkType::Folder | crate::api::links::LinkType::Album => {
                let folder_dto = link_details
                    .folder
                    .clone()
                    .or(link_details.album.clone())
                    .ok_or_else(|| anyhow::anyhow!("Folder DTO missing"))?;
                let decryption = crate::node::crypto::NodeCrypto::decrypt_folder(
                    account_client,
                    &link_dto,
                    &folder_dto.hash_key,
                    parent_key_result,
                )
                .await;

                let uid = NodeUid::new(_volume_id.clone(), link_dto.id.clone());
                let parent_uid = link_dto
                    .parent_id
                    .map(|id| NodeUid::new(_volume_id.clone(), id));

                if let (Ok(name), Ok(_hash_key), Ok(node_key)) = (
                    &decryption.link.name,
                    &decryption.hash_key,
                    &decryption.link.node_key,
                ) {
                    let node_base = NodeBase {
                        uid: uid.clone(),
                        parent_uid,
                        name: name.data.clone(),
                        creation_time: link_dto.creation_time,
                        trash_time: link_dto.trash_time,
                        name_author: PotentialObject::Node(crate::author::Author {
                            email_address: link_dto.name_signature_email_address.clone(),
                        }),
                        author: PotentialObject::Node(crate::author::Author {
                            email_address: link_dto.signature_email_address.clone(),
                        }),
                        owned_by: link_dto.owned_by.as_ref().map(|o| OwnedBy {
                            email: o.email.clone(),
                            organisation: o.organization.clone(),
                        }),
                    };

                    let passphrase_session_key = decryption
                        .link
                        .passphrase
                        .as_ref()
                        .map(|p| p.data.clone())
                        .map_err(|e| {
                            anyhow::anyhow!("Failed to decrypt folder passphrase: {}", e)
                        })?;

                    let is_anonymous = link_dto.signature_email_address.is_none();
                    let secrets = FolderSecrets {
                        base: NodeSecrets {
                            key: node_key.clone(),
                            passphrase_session_key: passphrase_session_key.clone(),
                            name_session_key: name.session_key.as_ref().map(|sk| PgpSessionKey {
                                algorithm: u8::from(sk.algorithm().unwrap_or(proton_rpgp::pgp::crypto::sym::SymmetricKeyAlgorithm::AES128)),
                                key: sk.as_ref().to_vec(),
                            }).unwrap_or_else(|| PgpSessionKey { algorithm: 9, key: vec![0; 32] }),
                            passphrase_for_anonymous_move: if is_anonymous { Some(passphrase_session_key.key) } else { None },
                        },
                        hash_key: _hash_key.data.clone(),
                    };

                    Ok(PotentialObject::Node(NodeMetadata {
                        inner: NodeAndSecrets::Folder(FolderNode { base: node_base }, secrets),
                        membership_share_id: link_details.sharing.as_ref().map(|s| s.share_id.clone()),
                        name_hash_digest: link_dto.name_hash_digest,
                    }))
                } else {
                    Err(anyhow::anyhow!("Decryption failed for folder"))
                }
            }
            crate::api::links::LinkType::File => {
                let file_dto = link_details
                    .file
                    .ok_or_else(|| anyhow::anyhow!("File DTO missing"))?;

                let decryption = crate::node::crypto::NodeCrypto::decrypt_file(
                    account_client,
                    &link_dto,
                    &file_dto,
                    parent_key_result,
                )
                .await;

                let uid = NodeUid::new(_volume_id.clone(), link_dto.id.clone());
                let parent_uid = link_dto.parent_id.map(|id| NodeUid::new(_volume_id, id));

                if let (Ok(name), Ok(_content_key), Ok(node_key)) = (
                    decryption.link.name,
                    decryption.content_key,
                    decryption.link.node_key,
                ) {
                    let node_base = NodeBase {
                        uid: uid.clone(),
                        parent_uid,
                        name: name.data,
                        creation_time: link_dto.creation_time,
                        trash_time: link_dto.trash_time,
                        name_author: PotentialObject::Node(crate::author::Author {
                            email_address: link_dto.name_signature_email_address.clone(),
                        }),
                        author: PotentialObject::Node(crate::author::Author {
                            email_address: link_dto.signature_email_address.clone(),
                        }),
                        owned_by: link_dto.owned_by.as_ref().map(|o| OwnedBy {
                            email: o.email.clone(),
                            organisation: o.organization.clone(),
                        }),
                    };

                    let passphrase_session_key = decryption
                        .link
                        .passphrase
                        .as_ref()
                        .map(|p| p.data.clone())
                        .map_err(|e| {
                            anyhow::anyhow!("Failed to decrypt file passphrase: {}", e)
                        })?;

                    let is_anonymous = link_dto.signature_email_address.is_none();
                    let secrets = FileSecrets {
                        base: NodeSecrets {
                            key: node_key.clone(),
                            passphrase_session_key: passphrase_session_key.clone(),
                            name_session_key: name.session_key.as_ref().map(|sk| PgpSessionKey {
                                algorithm: u8::from(sk.algorithm().unwrap_or(proton_rpgp::pgp::crypto::sym::SymmetricKeyAlgorithm::AES128)),
                                key: sk.as_ref().to_vec(),
                            }).unwrap_or_else(|| PgpSessionKey { algorithm: 9, key: vec![0; 32] }),
                            passphrase_for_anonymous_move: if is_anonymous { Some(passphrase_session_key.key) } else { None },
                        },
                        content_key: _content_key.data,
                    };

                    Ok(PotentialObject::Node(NodeMetadata {
                        inner: NodeAndSecrets::File(
                            FileNode {
                                base: FileOrFileDraftNode {
                                    base: node_base,
                                    media_type: file_dto.media_type.clone(),
                                },
                                active_revision: crate::node::revision::Revision {
                                    uid: crate::node::revision::RevisionUid {
                                        node_uid: uid.clone(),
                                        revision_id: file_dto
                                            .active_revision
                                            .as_ref()
                                            .map(|r| r.id.clone())
                                            .unwrap_or_default(),
                                    },
                                    creation_time: file_dto
                                        .active_revision
                                        .as_ref()
                                        .map(|r| r.creation_time)
                                        .unwrap_or_else(|| Utc::now()),
                                    size_on_cloud_storage: file_dto
                                        .active_revision
                                        .as_ref()
                                        .map(|r| r.storage_quota_consumption)
                                        .unwrap_or(0),
                                    claimed_size: Some(
                                        file_dto
                                            .active_revision
                                            .as_ref()
                                            .map(|r| r.storage_quota_consumption)
                                            .unwrap_or(0),
                                    ),
                                    claimed_digests: crate::node::file::FileContentDigests {
                                        sha1: None,
                                    },
                                    claimed_modification_time: Some(
                                        file_dto
                                            .active_revision
                                            .as_ref()
                                            .map(|r| r.creation_time)
                                            .unwrap_or_else(|| Utc::now()),
                                    ),
                                    thumbnails: vec![],
                                    additional_claimed_metadata: None,
                                    content_author: Some(decryption.content_authorship_claim.to_potential_author()),
                                },
                                total_size_on_cloud_storage: file_dto.total_size_on_storage,
                            },
                            secrets,
                        ),
                        membership_share_id: link_details.sharing.as_ref().map(|s| s.share_id.clone()),
                        name_hash_digest: link_dto.name_hash_digest,
                    }))
                } else {
                    Err(anyhow::anyhow!("Decryption failed for file"))
                }
            }
        }
    }
}
