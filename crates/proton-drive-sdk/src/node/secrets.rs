use crate::pgp::{PgpPrivateKey, PgpSessionKey};
use crate::share::Share;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DegradedNodeSecrets {
    pub key: Option<PgpPrivateKey>,
    pub passphrase_session_key: Option<PgpSessionKey>,
    pub name_session_key: Option<PgpSessionKey>,
}

pub struct ShareAndKey {
    pub share: Share,
    pub key: PgpPrivateKey,
}
