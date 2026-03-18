use serde_repr::{Deserialize_repr, Serialize_repr};

#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub r#type: ThumbnailType,
    pub content: Vec<u8>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThumbnailHeader {
    pub id: String,
    pub r#type: ThumbnailType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum ThumbnailType {
    Thumbnail = 1,
    Preview   = 2,
}