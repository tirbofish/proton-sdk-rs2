//! Easy Switch import helpers.
//!
//! Seeds a crypto cache with an already-decrypted import-folder key so a
//! public-link client can create children without the parent volume key.

use crate::author::Author;
use crate::cache::secret::DriveSecretCache;
use crate::crypto::CryptoGenerator;
use crate::node::authorship::AuthorshipClaim;
use crate::node::crypto::NodeCrypto;
use crate::node::folder::FolderSecrets;
use crate::node::{NodeSecrets, NodeUid};
use crate::pgp::{PgpArmoredMessage, PgpPrivateKey};
use crate::utils::PotentialObject;

#[derive(Debug, Clone)]
pub struct ImportFolder {
    pub node_uid: NodeUid,
    pub key: PgpPrivateKey,
    pub passphrase: String,
    pub armored_hash_key: PgpArmoredMessage,
}

pub async fn seed_import_folder_crypto_cache(
    cache: &dyn DriveSecretCache,
    import_folder: ImportFolder,
) -> anyhow::Result<()> {
    let claim = AuthorshipClaim {
        keys: vec![],
        author: Author::ANONYMOUS,
        key_retrieval_error_message: None,
    };
    let hash_key = NodeCrypto::decrypt_hash_key(
        Some(&import_folder.armored_hash_key),
        Some(&import_folder.key),
        &claim,
    )
    .map_err(|error| anyhow::anyhow!(error.unwrap_or_else(|| "folder hash key missing".into())))?;

    cache
        .set_folder_secrets(
            import_folder.node_uid,
            PotentialObject::Node(FolderSecrets {
                base: NodeSecrets {
                    key: import_folder.key,
                    passphrase_session_key: CryptoGenerator::generate_session_key(),
                    passphrase_pgp_session_key: None,
                    name_session_key: CryptoGenerator::generate_session_key(),
                    passphrase_for_anonymous_move: Some(import_folder.passphrase.into_bytes()),
                },
                hash_key: hash_key.data,
            }),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::secret::DefaultDriveSecretCache;
    use proton_sdk_rs2::cache::InMemoryCacheRepository;
    use std::sync::Arc;

    #[tokio::test]
    async fn decrypts_the_hash_key_and_seeds_folder_secrets() {
        let key = CryptoGenerator::generate_private_key().unwrap();
        let hash_key = CryptoGenerator::generate_folder_hash_key();
        let armored = NodeCrypto::encrypt_folder_hash_key(&key, &hash_key, &key).unwrap();
        let cache = DefaultDriveSecretCache::new(Arc::new(InMemoryCacheRepository::new()));
        let uid = NodeUid::from_parts("volumeId", "linkId");

        seed_import_folder_crypto_cache(
            &cache,
            ImportFolder {
                node_uid: uid.clone(),
                key: key.clone(),
                passphrase: "passphrase".into(),
                armored_hash_key: armored,
            },
        )
        .await
        .unwrap();

        let seeded = cache
            .try_get_folder_secrets(uid)
            .await
            .unwrap()
            .unwrap()
            .result()
            .unwrap();
        assert_eq!(seeded.hash_key, hash_key);
        assert_eq!(
            seeded.base.passphrase_for_anonymous_move.as_deref(),
            Some(b"passphrase".as_slice())
        );
        assert!(seeded.base.passphrase_pgp_session_key.is_none());
    }
}
