#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct LinkId(String);

impl Default for LinkId {
    fn default() -> Self {
        Self(String::new())
    }
}

impl LinkId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn raw(&self) -> &str {
        &self.0
    }
}
