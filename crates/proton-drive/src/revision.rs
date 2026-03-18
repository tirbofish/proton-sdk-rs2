#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default)]
#[serde(transparent)]
pub struct RevisionId(String);

impl RevisionId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn raw(&self) -> &str {
        &self.0
    }
}