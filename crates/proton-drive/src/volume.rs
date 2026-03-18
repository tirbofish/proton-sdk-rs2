use serde_repr::{Deserialize_repr, Serialize_repr};
use proton_sdk_rs2::utils::AsProtobuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct VolumeId(String);

impl Default for VolumeId {
    fn default() -> Self {
        Self(String::new())
    }
}

impl VolumeId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn raw(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum VolumeType {
    Main   = 1,
    Photos = 2,
}


#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum VolumeState {
    None     = 0,
    Active   = 1,
    Deleted  = 2,
    Locked   = 3,
    Restored = 4,
}

