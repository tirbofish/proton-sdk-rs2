use crate::node::authorship::AuthorshipClaim;
use crate::pgp::{
    PgpArmoredMessage, PgpArmoredPrivateKey, PgpArmoredSignature, PgpPrivateKey, PgpSessionKey,
};
use hmac::{Hmac, KeyInit, Mac};
use proton_rpgp::pgp::crypto::sym::SymmetricKeyAlgorithm;
use proton_rpgp::{
    AsPublicKeyRef, DataEncoding, Decryptor, Encryptor, ExternalDetachedSignature, PrivateKey,
    Signer,
};
use sha2::Sha256;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DecryptionError {
    pub message: String,
    pub authorship_verification_failure: Option<AuthorshipVerificationFailure>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuthorshipVerificationFailure {
    pub status: i32,
}

#[derive(Debug)]
pub struct DecryptionOutput<T> {
    pub data: T,
    pub session_key: Option<proton_rpgp::SessionKey>,
    pub authorship_verification_failure: Option<AuthorshipVerificationFailure>,
}

#[derive(Debug)]
pub struct LinkDecryptionResult {
    pub name: Result<DecryptionOutput<String>, Option<String>>,
    pub node_key: Result<PgpPrivateKey, String>,
    pub passphrase: Result<DecryptionOutput<PgpSessionKey>, String>,
    pub node_authorship_claim: AuthorshipClaim,
}

pub struct FileDecryptionResult {
    pub link: LinkDecryptionResult,
    pub content_key: Result<DecryptionOutput<PgpSessionKey>, Option<String>>,
    pub content_authorship_claim: AuthorshipClaim,
}

pub struct FolderDecryptionResult {
    pub link: LinkDecryptionResult,
    pub hash_key: Result<DecryptionOutput<Vec<u8>>, Option<String>>,
}

pub struct NodeCrypto;

impl NodeCrypto {
    pub fn encrypt_name(
        name: &str,
        session_key: &PgpSessionKey,
        parent_key: &PgpPrivateKey,
        signing_key: &PgpPrivateKey,
    ) -> anyhow::Result<PgpArmoredMessage> {
        let sk = session_key.to_rpgp_sk()?;
        let data = name.as_bytes();

        let encryptor = Encryptor::default()
            .with_session_key(sk)
            .with_encryption_key(parent_key.0.as_public_key())
            .with_signing_key(&signing_key.0);

        let result = encryptor.encrypt(data)?;
        let armored_bytes = result.armor()?;
        let armored = String::from_utf8(armored_bytes)?;
        Ok(PgpArmoredMessage(armored))
    }

    pub fn encrypt_and_sign_name(
        name: &str,
        hash_key: &[u8],
        parent_key: &PgpPrivateKey,
        signing_key: &PgpPrivateKey,
    ) -> anyhow::Result<(PgpArmoredMessage, Vec<u8>, PgpSessionKey)> {
        let session_key = crate::crypto::CryptoGenerator::generate_session_key();
        let sk = session_key.to_rpgp_sk()?;

        let mut hmac = HmacSha256::new_from_slice(hash_key)?;
        hmac.update(name.as_bytes());
        let name_hash_digest = hmac.finalize().into_bytes().to_vec();

        let encryptor = Encryptor::default()
            .with_session_key(sk)
            .with_encryption_key(parent_key.0.as_public_key())
            .with_signing_key(&signing_key.0);

        let result = encryptor.encrypt(name.as_bytes())?;
        let armored_bytes = result.armor()?;
        let armored = String::from_utf8(armored_bytes)?;

        Ok((PgpArmoredMessage(armored), name_hash_digest, session_key))
    }

    pub fn encrypt_and_sign_passphrase(
        passphrase: &[u8],
        parent_key: &PgpPrivateKey,
        signing_key: &PgpPrivateKey,
    ) -> anyhow::Result<(
        PgpArmoredMessage,
        Option<PgpArmoredSignature>,
        PgpSessionKey,
    )> {
        let session_key = crate::crypto::CryptoGenerator::generate_session_key();
        let sk = session_key.to_rpgp_sk()?;

        let encryptor = Encryptor::default()
            .with_session_key(sk)
            .with_encryption_key(parent_key.0.as_public_key())
            .with_signing_key(&signing_key.0);

        let result = encryptor.encrypt(passphrase)?;

        let signature_bytes = Signer::default()
            .with_signing_key(&signing_key.0)
            .sign_detached(passphrase, DataEncoding::Armored)?;

        let armored_result_bytes = result.armor()?;
        let armored_result = String::from_utf8(armored_result_bytes)?;
        let armored_sig = String::from_utf8(signature_bytes)?;

        Ok((
            PgpArmoredMessage(armored_result),
            Some(PgpArmoredSignature(armored_sig)),
            session_key,
        ))
    }

    /// Encrypts passphrase bytes for a new parent key WITHOUT signing (for regular, non-anonymous moves).
    pub fn reencrypt_passphrase(
        passphrase: &[u8],
        new_parent_key: &PgpPrivateKey,
    ) -> anyhow::Result<PgpArmoredMessage> {
        let session_key = crate::crypto::CryptoGenerator::generate_session_key();
        let sk = session_key.to_rpgp_sk()?;

        let encryptor = Encryptor::default()
            .with_session_key(sk)
            .with_encryption_key(new_parent_key.0.as_public_key());

        let result = encryptor.encrypt(passphrase)?;
        let armored_bytes = result.armor()?;
        let armored = String::from_utf8(armored_bytes)?;
        Ok(PgpArmoredMessage(armored))
    }

    pub fn encrypt_folder_hash_key(
        node_key: &PgpPrivateKey,
        hash_key: &[u8],
        signing_key: &PgpPrivateKey,
    ) -> anyhow::Result<PgpArmoredMessage> {
        let session_key = crate::crypto::CryptoGenerator::generate_session_key();
        let sk = session_key.to_rpgp_sk()?;

        let encryptor = Encryptor::default()
            .with_session_key(sk)
            .with_encryption_key(node_key.0.as_public_key())
            .with_signing_key(&signing_key.0);

        let result = encryptor.encrypt(hash_key)?;
        let armored_bytes = result.armor()?;
        let armored = String::from_utf8(armored_bytes)?;
        Ok(PgpArmoredMessage(armored))
    }

    pub async fn decrypt_file(
        account_client: Arc<dyn crate::account::AccountClient>,
        link: &crate::api::links::LinkDto,
        file: &crate::api::file::FileDto,
        parent_keys_result: Result<Vec<PgpPrivateKey>, String>,
    ) -> FileDecryptionResult {
        let link_decryption_result =
            Self::decrypt_link(account_client.clone(), link, parent_keys_result).await;

        let content_authorship_claim = crate::node::authorship::AuthorshipClaim::create(
            account_client,
            file.active_revision
                .as_ref()
                .and_then(|r| r.signature_email_address.as_deref()),
        )
        .await;

        let node_key = link_decryption_result.node_key.as_ref().ok();
        let content_key_result = Self::decrypt_content_key(
            node_key,
            &file.content_key_packet,
            file.content_key_signature.as_ref(),
            &content_authorship_claim,
        );

        FileDecryptionResult {
            link: link_decryption_result,
            content_key: content_key_result,
            content_authorship_claim,
        }
    }

    pub async fn decrypt_folder(
        account_client: Arc<dyn crate::account::AccountClient>,
        link: &crate::api::links::LinkDto,
        folder_hash_key: &PgpArmoredMessage,
        parent_keys_result: Result<Vec<PgpPrivateKey>, String>,
    ) -> FolderDecryptionResult {
        let link_decryption_result =
            Self::decrypt_link(account_client, link, parent_keys_result).await;

        let node_key = link_decryption_result.node_key.as_ref().ok();
        let hash_key_result = Self::decrypt_hash_key(
            Some(folder_hash_key),
            node_key,
            &link_decryption_result.node_authorship_claim,
        );

        FolderDecryptionResult {
            link: link_decryption_result,
            hash_key: hash_key_result,
        }
    }

    pub fn decrypt_hash_key(
        encrypted_hash_key: Option<&PgpArmoredMessage>,
        node_key: Option<&PgpPrivateKey>,
        authorship_claim: &AuthorshipClaim,
    ) -> Result<DecryptionOutput<Vec<u8>>, Option<String>> {
        let node_key = match node_key {
            Some(k) => k,
            None => return Err(None),
        };

        let encrypted_hash_key = match encrypted_hash_key {
            Some(k) => k,
            None => return Err(Some("Folder information missing".to_string())),
        };

        match Self::decrypt_message(
            encrypted_hash_key,
            None,
            std::iter::once(node_key),
            authorship_claim,
        ) {
            Ok((data, session_key, failure)) => Ok(DecryptionOutput {
                data,
                session_key,
                authorship_verification_failure: failure,
            }),
            Err(e) => Err(Some(e)),
        }
    }

    pub fn decrypt_content_key(
        node_key: Option<&PgpPrivateKey>,
        content_key_packet: &[u8],
        _content_key_signature: Option<&PgpArmoredSignature>,
        _node_authorship_claim: &AuthorshipClaim,
    ) -> Result<DecryptionOutput<PgpSessionKey>, Option<String>> {
        let node_key = match node_key {
            Some(k) => k,
            None => return Err(None),
        };

        match Decryptor::default()
            .with_decryption_key(&node_key.0)
            .decrypt_session_key(content_key_packet)
        {
            Ok(sk) => Ok(DecryptionOutput {
                data: PgpSessionKey {
                    algorithm: u8::from(sk.algorithm().unwrap_or(SymmetricKeyAlgorithm::AES128)),
                    key: sk.as_ref().to_vec(),
                },
                session_key: Some(sk),
                authorship_verification_failure: None,
            }),
            Err(e) => Err(Some(e.to_string())),
        }
    }

    pub async fn decrypt_link(
        account_client: Arc<dyn crate::account::AccountClient>,
        link: &crate::api::links::LinkDto,
        parent_keys_result: Result<Vec<PgpPrivateKey>, String>,
    ) -> LinkDecryptionResult {
        let node_authorship_claim =
            crate::node::authorship::NodeAuthorshipClaimProvider::get_node_authorship_claim(
                account_client,
                link,
            )
            .await;

        let passphrase_output = match &parent_keys_result {
            Ok(pks) => Self::decrypt_passphrase(
                pks,
                &link.passphrase,
                link.passphrase_signature.as_ref(),
                &node_authorship_claim,
            ),
            Err(e) => Err(e.clone()),
        };

        let node_key = match &passphrase_output {
            Ok(po) => match Self::unlock_key_with_passphrase(&link.key, &po.data.key) {
                Ok(k) => Ok(k),
                Err(e) => Err(format!("Failed to unlock node key: {}", e)),
            },
            Err(e) => Err(format!("Failed to decrypt node passphrase: {}", e)),
        };

        let name = match (&node_key, &parent_keys_result) {
            (Ok(nk), Ok(pks)) => {
                let mut keys: Vec<&PgpPrivateKey> = vec![nk];
                for pk in pks {
                    keys.push(pk);
                }
                match Self::decrypt_message(&link.name, None, keys, &node_authorship_claim) {
                    Ok((data, session_key, failure)) => Ok(DecryptionOutput {
                        data: String::from_utf8_lossy(&data).to_string(),
                        session_key,
                        authorship_verification_failure: failure,
                    }),
                    Err(e) => Err(Some(e)),
                }
            }
            (Ok(nk), _) => match Self::decrypt_message(
                &link.name,
                None,
                std::iter::once(nk),
                &node_authorship_claim,
            ) {
                Ok((data, session_key, failure)) => Ok(DecryptionOutput {
                    data: String::from_utf8_lossy(&data).to_string(),
                    session_key,
                    authorship_verification_failure: failure,
                }),
                Err(e) => Err(Some(e)),
            },
            _ => Err(None),
        };

        LinkDecryptionResult {
            name,
            node_key,
            passphrase: passphrase_output,
            node_authorship_claim,
        }
    }

    pub fn unlock_key_with_passphrase(
        armored_key: &PgpArmoredPrivateKey,
        passphrase: &[u8],
    ) -> Result<PgpPrivateKey, String> {
        match PrivateKey::import(armored_key.0.as_bytes(), passphrase, DataEncoding::Auto) {
            Ok(k) => Ok(PgpPrivateKey(k)),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn decrypt_passphrase<'a>(
        node_keys: impl IntoIterator<Item = &'a PgpPrivateKey>,
        encrypted_passphrase: &PgpArmoredMessage,
        passphrase_signature: Option<&PgpArmoredSignature>,
        authorship_claim: &AuthorshipClaim,
    ) -> Result<DecryptionOutput<PgpSessionKey>, String> {
        match Self::decrypt_message(
            encrypted_passphrase,
            passphrase_signature,
            node_keys,
            authorship_claim,
        ) {
            Ok((data, session_key, failure)) => {
                if !data.is_empty() {
                    return Ok(DecryptionOutput {
                        data: PgpSessionKey {
                            algorithm: 9,
                            key: data,
                        },
                        session_key,
                        authorship_verification_failure: failure,
                    });
                }

                let sk = session_key.ok_or_else(|| "Session key missing".to_string())?;
                Ok(DecryptionOutput {
                    data: PgpSessionKey {
                        algorithm: u8::from(
                            sk.algorithm().unwrap_or(SymmetricKeyAlgorithm::AES128),
                        ),
                        key: sk.as_ref().to_vec(),
                    },
                    session_key: Some(sk),
                    authorship_verification_failure: failure,
                })
            }
            Err(e) => Err(e),
        }
    }

    pub fn decrypt_message<'a>(
        encrypted_message: &PgpArmoredMessage,
        detached_signature: Option<&PgpArmoredSignature>,
        decryption_keys: impl IntoIterator<Item = &'a PgpPrivateKey>,
        authorship_claim: &AuthorshipClaim,
    ) -> Result<
        (
            Vec<u8>,
            Option<proton_rpgp::SessionKey>,
            Option<AuthorshipVerificationFailure>,
        ),
        String,
    > {
        let reader = encrypted_message.0.as_bytes();
        let unarmored_buffer = if encrypted_message.0.contains("-----BEGIN PGP MESSAGE-----") {
            proton_rpgp::armor::unarmor(reader).map_err(|e| e.to_string())?
        } else {
            reader.to_vec()
        };

        let mut decryptor = Decryptor::default();
        for key in decryption_keys {
            decryptor = decryptor.with_decryption_key(&key.0);
        }

        let session_key = decryptor
            .decrypt_session_key(&unarmored_buffer)
            .map_err(|e| e.to_string())?;

        let mut decryptor = Decryptor::default().with_session_key(session_key.clone());

        if let Some(sig) = detached_signature {
            decryptor = decryptor.with_external_detached_signature(
                ExternalDetachedSignature::new_unencrypted(sig.0.as_bytes(), DataEncoding::Auto),
            );
        }

        for key in &authorship_claim.keys {
            decryptor = decryptor.with_verification_key(key);
        }

        match decryptor.decrypt(&unarmored_buffer, DataEncoding::Auto) {
            Ok(result) => {
                let failure = if !authorship_claim.keys.is_empty() {
                    if !result.verification_result.is_ok() {
                        Some(AuthorshipVerificationFailure { status: 1 }) // 1 for failure
                    } else {
                        None
                    }
                } else {
                    None
                };

                Ok((result.data, Some(session_key), failure))
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

type HmacSha256 = Hmac<Sha256>;
