#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AdditionalMetadataProperty {
    pub key: String,
    pub value: String,
}
