pub mod client;
pub mod entity;
pub mod secret;

use crate::node::{DegradedNode, Node};
use crate::share::ShareId;
use crate::utils::PotentialObject;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct CachedNodeInfo {
    pub node_provision_result: PotentialObject<Node, DegradedNode>,
    pub membership_share_id: Option<ShareId>,
    pub name_hash_digest: Vec<u8>,
}
