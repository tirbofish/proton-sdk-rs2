use std::sync::Arc;

use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::{api::ResponseCode, auth::AuthenticationApiClientTrait, secret::SessionSecretCache};

pub mod proton {
    include!(concat!(env!("OUT_DIR"), "/proton.sdk.rs"));
}

mod session;
mod client;
mod secret;
mod api;
mod auth;
mod cache;

#[derive(Debug, Clone, PartialEq)]
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

pub struct EventId(String);

pub enum PasswordMode
{
    Single = 1,
    Dual = 2,
}
