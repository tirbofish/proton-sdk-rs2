//! A wrapper for caches which force a AES-256-GCM encryption on all values.
//!
//! Requires the `cache-encrypted` feature to be enabled.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::Context;
use futures::stream::BoxStream;
use futures::StreamExt;
use hmac::{Hmac, KeyInit, Mac};
use proton_sdk_rs2::cache::CacheRepository;
use rand::RngCore;
use sha2::Sha256;
use std::sync::Arc;

type HmacSha256 = Hmac<Sha256>;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;

/// Context label used during HKDF key derivation.  Binding the derived key to
/// this label prevents cross-context key reuse.
const ENCRYPTION_CONTEXT: &[u8] = b"Drive.EncryptedCacheRepository";

/// A [`CacheRepository`] decorator that transparently encrypts all stored
/// values using AES-256-GCM.
///
/// Each value is encrypted with a unique key derived from the master key, a
/// random 16-byte salt, and the entry's own cache key.  The derivation is
/// performed with HKDF-SHA-256 so that two entries sharing the same master key
/// and plaintext value will still produce distinct ciphertexts.
///
/// The wire format for each stored blob is:
/// ```text
/// [ salt (16 bytes) | ciphertext (variable) | GCM tag (16 bytes) ]
/// ```
/// encoded as Base64.
///
/// If the authentication tag fails to verify on read (e.g. the master key has
/// rotated or the stored data has been corrupted), the entire cache is cleared
/// and a cache miss is returned rather than propagating an error.
pub struct EncryptedCacheRepository {
    inner: Arc<dyn CacheRepository>,
    master_key: Vec<u8>,
}

impl EncryptedCacheRepository {
    /// Wraps an existing [`CacheRepository`] with AES-256-GCM encryption.
    ///
    /// # Arguments
    ///
    /// * `inner` – The underlying repository that will store the ciphertext.
    /// * `master_key` – Raw key material used as the HKDF input keying
    ///   material.  Any length is accepted; 32 bytes or more is recommended.
    pub fn new(inner: Arc<dyn CacheRepository>, master_key: Vec<u8>) -> Self {
        Self { inner, master_key }
    }

    fn encrypt(&self, entry_key: &str, plaintext: &str) -> anyhow::Result<String> {
        let mut salt = [0u8; SALT_LEN];
        rand::thread_rng().fill_bytes(&mut salt);

        let info: Vec<u8> = ENCRYPTION_CONTEXT
            .iter()
            .chain(entry_key.as_bytes())
            .copied()
            .collect();

        let okm = hkdf_sha256(&salt, &self.master_key, &info, KEY_LEN + NONCE_LEN)?;
        let (key_bytes, nonce_bytes) = okm.split_at(KEY_LEN);

        let cipher = <Aes256Gcm as aes_gcm::aead::KeyInit>::new_from_slice(key_bytes)
            .context("Invalid AES key length")?;
        let nonce = Nonce::from_slice(nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|_| anyhow::anyhow!("AES-GCM encryption failed"))?;

        let mut blob = Vec::with_capacity(SALT_LEN + ciphertext.len());
        blob.extend_from_slice(&salt);
        blob.extend_from_slice(&ciphertext);

        Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &blob,
        ))
    }

    fn decrypt(&self, entry_key: &str, encoded: &str) -> anyhow::Result<Option<String>> {
        let blob = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            encoded,
        )
        .context("Failed to decode Base64 ciphertext")?;

        if blob.len() < SALT_LEN + TAG_LEN {
            anyhow::bail!("Encrypted blob is too short to be valid");
        }

        let (salt, ciphertext) = blob.split_at(SALT_LEN);

        let info: Vec<u8> = ENCRYPTION_CONTEXT
            .iter()
            .chain(entry_key.as_bytes())
            .copied()
            .collect();

        let okm = hkdf_sha256(salt, &self.master_key, &info, KEY_LEN + NONCE_LEN)?;
        let (key_bytes, nonce_bytes) = okm.split_at(KEY_LEN);

        let cipher = <Aes256Gcm as aes_gcm::aead::KeyInit>::new_from_slice(key_bytes)
            .context("Invalid AES key length")?;
        let nonce = Nonce::from_slice(nonce_bytes);

        match cipher.decrypt(nonce, ciphertext) {
            Ok(plaintext_bytes) => {
                let plaintext =
                    String::from_utf8(plaintext_bytes).context("Decrypted bytes are not UTF-8")?;
                Ok(Some(plaintext))
            }
            Err(_) => Ok(None),
        }
    }
}

#[async_trait::async_trait]
impl CacheRepository for EncryptedCacheRepository {
    /// Encrypts `value` before delegating to the inner repository.
    async fn set(&self, key: &str, value: String, tags: Vec<String>) -> anyhow::Result<()> {
        let encrypted = self.encrypt(key, &value)?;
        self.inner.set(key, encrypted, tags).await
    }

    async fn remove(&self, key: &str) -> anyhow::Result<()> {
        self.inner.remove(key).await
    }

    async fn remove_by_tag(&self, tag: &str) -> anyhow::Result<()> {
        self.inner.remove_by_tag(tag).await
    }

    async fn clear(&self) -> anyhow::Result<()> {
        self.inner.clear().await
    }

    /// Retrieves and decrypts a value.
    ///
    /// Returns `None` both when the key is absent and when the authentication
    /// tag does not match (which indicates key rotation or tampered data).
    /// In the latter case the entire cache is cleared to avoid stale
    /// ciphertexts accumulating under a new key.
    async fn try_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let Some(encrypted) = self.inner.try_get(key).await? else {
            return Ok(None);
        };

        match self.decrypt(key, &encrypted)? {
            Some(plaintext) => Ok(Some(plaintext)),
            None => {
                self.inner.clear().await?;
                Ok(None)
            }
        }
    }

    /// Retrieves and decrypts all entries matching every tag in `tags`.
    ///
    /// If any entry fails authentication the entire cache is cleared and the
    /// stream terminates early.
    fn get_by_tags(&self, tags: Vec<String>) -> BoxStream<'_, anyhow::Result<(String, String)>> {
        let stream = self.inner.get_by_tags(tags);

        let mapped = stream.then(move |item| async move {
            let (key, encrypted) = item?;
            match self.decrypt(&key, &encrypted)? {
                Some(plaintext) => Ok((key, plaintext)),
                None => {
                    self.inner.clear().await?;
                    Err(anyhow::anyhow!(
                        "Authentication tag mismatch: cache cleared"
                    ))
                }
            }
        });

        mapped.boxed()
    }
}

/// Performs HKDF-SHA256 (RFC 5869) extract-then-expand.
///
/// Derives `length` bytes of output keying material from `ikm` using the
/// provided `salt` and `info` binding label.
fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], length: usize) -> anyhow::Result<Vec<u8>> {
    const HASH_LEN: usize = 32;

    let mut mac =
        HmacSha256::new_from_slice(salt).map_err(|_| anyhow::anyhow!("HKDF extract failed"))?;
    mac.update(ikm);
    let prk = mac.finalize().into_bytes();

    let n = length.div_ceil(HASH_LEN);
    anyhow::ensure!(n <= 255, "HKDF requested output length exceeds maximum");

    let mut okm = Vec::with_capacity(n * HASH_LEN);
    let mut t: Vec<u8> = Vec::new();

    for i in 1..=(n as u8) {
        let mut mac = HmacSha256::new_from_slice(&prk)
            .map_err(|_| anyhow::anyhow!("HKDF expand failed"))?;
        mac.update(&t);
        mac.update(info);
        mac.update(&[i]);
        t = mac.finalize().into_bytes().to_vec();
        okm.extend_from_slice(&t);
    }

    okm.truncate(length);
    Ok(okm)
}
