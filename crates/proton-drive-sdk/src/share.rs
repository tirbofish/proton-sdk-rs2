#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ShareId(String);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Share {
    pub id: crate::share::ShareId,
    pub root_folder_id: crate::node::NodeUid,
    pub membership_address_id: crate::account::AddressId,
    pub share_type: crate::api::share::ShareType,
}

impl Default for ShareId {
    fn default() -> Self {
        Self(String::new())
    }
}

impl ShareId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn raw(&self) -> &String {
        &self.0
    }
}
