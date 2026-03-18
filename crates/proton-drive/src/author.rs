#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Author {
    pub email_address: Option<String>,
}

impl Author {
    pub const ANONYMOUS: Author = Author { email_address: None };

    pub fn try_get_identity(&self) -> Option<&str> {
        self.email_address.as_deref()
    }

    pub fn is_anonymous(&self) -> bool {
        self.email_address.is_none()
    }
}