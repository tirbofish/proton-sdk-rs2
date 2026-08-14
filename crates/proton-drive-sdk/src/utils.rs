use crate::node::file::{DegradedFileMetadata, FileMetadata};
use crate::node::folder::{DegradedFolderMetadata, FolderMetadata};
use crate::node::{DegradedNodeMetadata, NodeMetadata};
use ::serde::{Deserialize, Serialize};
pub mod batch;
pub mod semaphore;
pub mod serde;
#[cfg(feature = "thumbnail-generation")]
pub mod thumbnail;

pub struct AlternateFileNameGenerator;

impl AlternateFileNameGenerator {
    pub fn get_names(file_name: &str) -> Vec<String> {
        let (stem, extension) = split_name_and_extension(file_name);
        let mut candidates = Vec::with_capacity(64);

        for index in 1..=64 {
            let candidate = if extension.is_empty() {
                format!("{} ({})", stem, index)
            } else {
                format!("{} ({}).{}", stem, index, extension)
            };
            candidates.push(candidate);
        }

        candidates
    }
}

fn split_name_and_extension(file_name: &str) -> (&str, &str) {
    if let Some((stem, extension)) = file_name.rsplit_once('.') {
        if !stem.is_empty() && !extension.is_empty() {
            return (stem, extension);
        }
    }
    (file_name, "")
}

/// An enum used to show between a Node and a DegradedNode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PotentialObject<N, DN> {
    Node(N),
    Degraded(DN),
}

impl<N, DN> PotentialObject<N, DN> {
    pub fn map<F, M>(self, f: F) -> PotentialObject<M, DN>
    where
        F: FnOnce(N) -> M,
    {
        match self {
            PotentialObject::Node(n) => PotentialObject::Node(f(n)),
            PotentialObject::Degraded(dn) => PotentialObject::Degraded(dn),
        }
    }

    pub fn map_degraded<F, DM>(self, f: F) -> PotentialObject<N, DM>
    where
        F: FnOnce(DN) -> DM,
    {
        match self {
            PotentialObject::Node(n) => PotentialObject::Node(n),
            PotentialObject::Degraded(dn) => PotentialObject::Degraded(f(dn)),
        }
    }

    pub fn map_both<F1, M, F2, DM>(self, f1: F1, f2: F2) -> PotentialObject<M, DM>
    where
        F1: FnOnce(N) -> M,
        F2: FnOnce(DN) -> DM,
    {
        match self {
            PotentialObject::Node(n) => PotentialObject::Node(f1(n)),
            PotentialObject::Degraded(dn) => PotentialObject::Degraded(f2(dn)),
        }
    }
}

impl<N, DN> Default for PotentialObject<N, DN>
where
    N: Default,
{
    fn default() -> Self {
        PotentialObject::Node(N::default())
    }
}

impl PotentialObject<FolderMetadata, DegradedFolderMetadata> {
    pub fn folder_to_node_metadata(self) -> PotentialObject<NodeMetadata, DegradedNodeMetadata> {
        match self {
            PotentialObject::Node(n) => PotentialObject::Node(NodeMetadata::from_folder(n)),
            PotentialObject::Degraded(d) => {
                PotentialObject::Degraded(DegradedNodeMetadata::from_folder(d))
            }
        }
    }
}

impl PotentialObject<FileMetadata, DegradedFileMetadata> {
    pub fn file_to_node_metadata(self) -> PotentialObject<NodeMetadata, DegradedNodeMetadata> {
        match self {
            PotentialObject::Node(n) => PotentialObject::Node(NodeMetadata::from_file(n)),
            PotentialObject::Degraded(d) => {
                PotentialObject::Degraded(DegradedNodeMetadata::from_file(d))
            }
        }
    }
}

impl<N, DN> PotentialObject<N, DN> {
    pub fn unwrap(self) -> N {
        match self {
            PotentialObject::Node(n) => n,
            PotentialObject::Degraded(_) => {
                panic!("Unwrapping PotentialObject returns degraded object")
            }
        }
    }

    pub fn result(self) -> anyhow::Result<N> {
        match self {
            PotentialObject::Node(n) => Ok(n),
            PotentialObject::Degraded(_) => {
                anyhow::bail!("Unwrapping PotentialObject returns degraded object")
            }
        }
    }

    pub fn ok(self) -> Option<N> {
        match self {
            PotentialObject::Node(node) => Some(node),
            PotentialObject::Degraded(_) => None,
        }
    }

    pub fn err(self) -> Option<DN> {
        match self {
            PotentialObject::Node(_) => None,
            PotentialObject::Degraded(node) => Some(node),
        }
    }
}

impl<N, DN> PotentialObject<N, DN> {
    pub fn is_ok(&self) -> bool {
        matches!(self, PotentialObject::Node(_))
    }

    pub fn as_ref(&self) -> PotentialObject<&N, &DN> {
        match self {
            PotentialObject::Node(n) => PotentialObject::Node(n),
            PotentialObject::Degraded(d) => PotentialObject::Degraded(d),
        }
    }

    pub fn as_mut(&mut self) -> PotentialObject<&mut N, &mut DN> {
        match self {
            PotentialObject::Node(n) => PotentialObject::Node(n),
            PotentialObject::Degraded(d) => PotentialObject::Degraded(d),
        }
    }
}

impl PotentialObject<NodeMetadata, DegradedNodeMetadata> {}
