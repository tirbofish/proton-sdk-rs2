use base64::{Engine, engine::general_purpose::STANDARD};
use proton_rpgp::{AsPublicKeyRef, DataEncoding, Decryptor, PrivateKey, Profile, PublicKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone)]
pub struct PgpPublicKey(pub PublicKey);

impl Serialize for PgpPublicKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let armored = self
            .0
            .export(DataEncoding::Armored)
            .map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&String::from_utf8_lossy(&armored))
    }
}

impl<'de> Deserialize<'de> for PgpPublicKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let key = PublicKey::import(s.as_bytes(), DataEncoding::Auto)
            .map_err(serde::de::Error::custom)?;
        Ok(PgpPublicKey(key))
    }
}

#[derive(Debug, Clone)]
pub struct PgpPrivateKey(pub PrivateKey);

impl PgpPrivateKey {
    pub fn decrypt_session_key(&self, content_key_packet: &[u8]) -> Result<PgpSessionKey, String> {
        match Decryptor::default()
            .with_decryption_key(&self.0)
            .decrypt_session_key(content_key_packet)
        {
            Ok(sk) => Ok(PgpSessionKey::from_rpgp(&sk)),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn encrypt_session_key(&self, session_key: &PgpSessionKey) -> anyhow::Result<Vec<u8>> {
        let sk = session_key.to_rpgp_sk()?;
        let encryptor =
            proton_rpgp::Encryptor::default().with_encryption_key(self.0.as_public_key());
        Ok(encryptor.encrypt_session_key(&sk)?)
    }

    /// Detached armored signature using SHA-256, matching Proton Drive / openpgp.js.
    pub fn sign_detached_armored(&self, data: &[u8]) -> anyhow::Result<PgpArmoredSignature> {
        let profile = proton_rpgp::ProfileSettings::builder()
            .preferred_hash_algorithm(proton_rpgp::pgp::crypto::hash::HashAlgorithm::Sha256)
            .build_into_profile();
        let bytes = proton_rpgp::Signer::new(profile)
            .with_signing_key(&self.0)
            .sign_detached(data, DataEncoding::Armored)?;
        Ok(PgpArmoredSignature(String::from_utf8(bytes)?))
    }

    pub fn to_armored_private_key(
        &self,
        passphrase: Option<&[u8]>,
    ) -> anyhow::Result<PgpArmoredPrivateKey> {
        let armored = match passphrase {
            Some(p) => self
                .0
                .export(&Profile::default(), p, DataEncoding::Armored)?,
            None => self.0.export_unlocked(DataEncoding::Armored)?,
        };
        Ok(PgpArmoredPrivateKey(String::from_utf8(armored)?))
    }
}

impl Serialize for PgpPrivateKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let armored = self
            .0
            .export_unlocked(DataEncoding::Armored)
            .map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&String::from_utf8_lossy(&armored))
    }
}

impl<'de> Deserialize<'de> for PgpPrivateKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let key = PrivateKey::import_unlocked(s.as_bytes(), DataEncoding::Auto)
            .map_err(serde::de::Error::custom)?;
        Ok(PgpPrivateKey(key))
    }
}

pub type PgpPublicKeyRing = Vec<PgpPublicKey>;

#[derive(Debug, Clone, Default)]
pub struct PgpArmoredMessage(pub String);

impl Serialize for PgpArmoredMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PgpArmoredMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(PgpArmoredMessage(s))
    }
}

#[derive(Debug, Clone)]
pub struct PgpArmoredSignature(pub String);

impl Serialize for PgpArmoredSignature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PgpArmoredSignature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(PgpArmoredSignature(s))
    }
}

#[derive(Debug, Clone)]
pub struct PgpArmoredPrivateKey(pub String);

impl Serialize for PgpArmoredPrivateKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PgpArmoredPrivateKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(PgpArmoredPrivateKey(s))
    }
}

#[derive(Debug, Clone)]
pub struct PgpSessionKey {
    pub algorithm: u8,
    pub key: Vec<u8>,
}

impl Serialize for PgpSessionKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Matching C# PgpSessionKeyJsonConverter logic: Base64(version/algo + key)
        let mut bytes = vec![self.algorithm];
        bytes.extend_from_slice(&self.key);
        serializer.serialize_str(&STANDARD.encode(bytes))
    }
}

impl<'de> Deserialize<'de> for PgpSessionKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;

        // Try Base64
        if let Ok(bytes) = STANDARD.decode(&s) {
            if bytes.len() == 33 {
                // Versioned: [version, key...]
                return Ok(PgpSessionKey {
                    algorithm: bytes[0],
                    key: bytes[1..].to_vec(),
                });
            } else if bytes.len() == 32 {
                // Unversioned: [key...]
                return Ok(PgpSessionKey {
                    algorithm: 9, // Default to AES256
                    key: bytes,
                });
            }
        }

        // Try Hex
        if let Ok(bytes) = hex::decode(&s) {
            if bytes.len() == 33 {
                return Ok(PgpSessionKey {
                    algorithm: bytes[0],
                    key: bytes[1..].to_vec(),
                });
            } else if bytes.len() == 32 {
                return Ok(PgpSessionKey {
                    algorithm: 9,
                    key: bytes,
                });
            }
        }

        // Fallback for any other lengths
        if let Ok(bytes) = STANDARD.decode(&s) {
            if bytes.len() > 33 {
                return Ok(PgpSessionKey {
                    algorithm: bytes[0],
                    key: bytes[1..].to_vec(),
                });
            } else if !bytes.is_empty() {
                return Ok(PgpSessionKey {
                    algorithm: 9,
                    key: bytes,
                });
            }
        }

        Err(serde::de::Error::custom("Invalid session key format"))
    }
}

impl PgpSessionKey {
    pub fn from_rpgp(sk: &proton_rpgp::SessionKey) -> Self {
        Self {
            algorithm: sk.algorithm().map(u8::from).unwrap_or(0),
            key: sk.as_ref().to_vec(),
        }
    }

    pub fn to_rpgp_sk(&self) -> anyhow::Result<proton_rpgp::SessionKey> {
        if self.algorithm == 0 {
            return Ok(proton_rpgp::SessionKey::new_for_seipdv2(&self.key));
        }
        let algo = proton_rpgp::pgp::crypto::sym::SymmetricKeyAlgorithm::from(self.algorithm);
        Ok(proton_rpgp::SessionKey::new(&self.key, algo))
    }

    /// Decrypt ciphertext that may be SEIPDv1 or SEIPDv2. Content-key packets from
    /// current Proton Drive clients are v6/SEIPDv2; reconstructing them with
    /// `SessionKey::new` (always v3/v4) fails with "v3/4 session keys are not allowed".
    pub fn decrypt(&self, ciphertext: &[u8]) -> anyhow::Result<Vec<u8>> {
        let algo = proton_rpgp::pgp::crypto::sym::SymmetricKeyAlgorithm::from(self.algorithm);
        let keys = [
            proton_rpgp::SessionKey::new_for_seipdv2(&self.key),
            proton_rpgp::SessionKey::new(&self.key, algo),
        ];
        let mut last_err = None;
        for sk in keys {
            match proton_rpgp::Decryptor::default()
                .with_session_key(sk)
                .decrypt(ciphertext, DataEncoding::Auto)
            {
                Ok(result) => return Ok(result.data),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .map(Into::into)
            .unwrap_or_else(|| anyhow::anyhow!("session key decrypt failed")))
    }
}
