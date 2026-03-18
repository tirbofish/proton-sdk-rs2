use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub mod protobuf {
    include!(concat!(env!("OUT_DIR"), "/proton.sdk.rs"));
}

pub mod account;
pub mod addresses;
pub mod api;
pub mod auth;
pub mod cache;
pub mod client;
pub mod keys;
pub mod secret;
pub mod session;
pub mod users;
pub mod utils;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn raw(&self) -> &String {
        &self.0
    }
}

impl ToString for SessionId {
    fn to_string(&self) -> String {
        self.0.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UserId(String);

impl UserId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn raw(&self) -> &String {
        &self.0
    }
}

impl ToString for UserId {
    fn to_string(&self) -> String {
        self.0.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EventId(String);

#[derive(Debug, Clone, Copy)]
pub enum PasswordMode {
    Single = 1,
    Dual = 2,
}

impl<'de> Deserialize<'de> for PasswordMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = i32::deserialize(deserializer)?;
        Ok(match value {
            2 => Self::Dual,
            _ => Self::Single,
        })
    }
}

impl Serialize for PasswordMode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let value = match self {
            Self::Single => 1,
            Self::Dual => 2,
        };
        serializer.serialize_i32(value)
    }
}
