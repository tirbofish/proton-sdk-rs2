use std::sync::Arc;
use proton_rpgp::{PublicKey, PrivateKey};
use crate::account::AccountClient;
use crate::author::Author;
use crate::utils::PotentialObject;

pub enum PgpKeyRingOrKey {
    KeyRing(Vec<PublicKey>),
    PrivateKey(PrivateKey),
}

#[derive(Debug, Clone)]
pub struct AuthorshipClaim {
    pub keys: Vec<PublicKey>,
    pub author: Author,
    pub key_retrieval_error_message: Option<String>,
}

impl AuthorshipClaim {
    pub async fn create(
        account_client: Arc<dyn AccountClient>,
        claimed_author_email_address: Option<&str>,
    ) -> Self {
        let email = match claimed_author_email_address {
            None | Some("") => {
                return Self {
                    keys: vec![],
                    author: Author::ANONYMOUS,
                    key_retrieval_error_message: None,
                };
            }
            Some(e) => e,
        };

        match account_client.get_address_public_keys(email).await {
            Ok(keys) => Self {
                keys,
                author: Author { email_address: Some(email.to_string()) },
                key_retrieval_error_message: None,
            },
            Err(e) => Self {
                keys: vec![],
                author: Author { email_address: Some(email.to_string()) },
                key_retrieval_error_message: Some(e.to_string()),
            },
        }
    }

    /// Returns the public key ring for non-anonymous authors,
    /// or wraps the fallback private key for anonymous authors.
    pub fn get_key_ring(&self, anonymous_fallback_key: &PrivateKey) -> PgpKeyRingOrKey {
        if !self.author.is_anonymous() {
            PgpKeyRingOrKey::KeyRing(self.keys.clone())
        } else {
            PgpKeyRingOrKey::PrivateKey(anonymous_fallback_key.clone())
        }
    }

    pub fn to_potential_author(&self) -> PotentialObject<Author, crate::protobuf::SignatureVerificationError> {
        // For now, assume we don't have verification errors here, but in reality we might.
        PotentialObject::Node(self.author.clone())
    }
}

pub struct NodeAuthorshipClaimProvider;

impl NodeAuthorshipClaimProvider {
    pub async fn get_node_authorship_claim(
        account_client: Arc<dyn AccountClient>,
        link: &crate::api::links::LinkDto,
    ) -> AuthorshipClaim {
        AuthorshipClaim::create(account_client, link.signature_email_address.as_deref()).await
    }

    pub async fn get_content_authorship_claim(
        account_client: Arc<dyn AccountClient>,
        revision: &crate::api::revision::RevisionDto,
    ) -> AuthorshipClaim {
        AuthorshipClaim::create(account_client, revision.signature_email_address.as_deref()).await
    }
}
