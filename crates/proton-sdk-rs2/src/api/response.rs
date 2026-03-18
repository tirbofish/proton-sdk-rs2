use crate::{EventId, PasswordMode, SessionId, UserId, api::ApiResponse};
use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Deserializer, de::Error as _};

fn deserialize_boolish<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Bool(v) => Ok(v),
        serde_json::Value::Number(n) => Ok(n.as_i64().unwrap_or(0) != 0),
        serde_json::Value::String(s) => {
            let lower = s.to_ascii_lowercase();
            if lower == "true" || lower == "yes" {
                return Ok(true);
            }
            if lower == "false" || lower == "no" {
                return Ok(false);
            }
            if let Ok(parsed) = lower.parse::<i64>() {
                return Ok(parsed != 0);
            }
            Err(D::Error::custom("cannot parse boolean-like string"))
        }
        serde_json::Value::Null => Ok(false),
        _ => Err(D::Error::custom("expected bool/int/string for boolean field")),
    }
}

fn deserialize_option_boolish<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(v)) => Ok(Some(v)),
        Some(serde_json::Value::Number(n)) => Ok(Some(n.as_i64().unwrap_or(0) != 0)),
        Some(serde_json::Value::String(s)) => {
            let lower = s.to_ascii_lowercase();
            if lower == "true" || lower == "yes" {
                return Ok(Some(true));
            }
            if lower == "false" || lower == "no" {
                return Ok(Some(false));
            }
            if let Ok(parsed) = lower.parse::<i64>() {
                return Ok(Some(parsed != 0));
            }
            Err(D::Error::custom("cannot parse optional boolean-like string"))
        }
        Some(_) => Err(D::Error::custom("expected bool/int/string for optional boolean field")),
    }
}

fn deserialize_binary_bytes<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(s) => general_purpose::STANDARD
            .decode(s.as_bytes())
            .map_err(|_| D::Error::custom("invalid base64 in KeySalt")),
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let b = item
                    .as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| D::Error::custom("KeySalt byte array must contain 0..255 integers"))?;
                out.push(b);
            }
            Ok(out)
        }
        serde_json::Value::Null => Ok(vec![]),
        _ => Err(D::Error::custom(
            "KeySalt must be base64 string or byte array",
        )),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SesisonInitiationResponse {
    #[serde(rename = "Version", alias = "version")]
    pub version: i32,
    #[serde(rename = "Modulus", alias = "modulus")]
    pub modulus: String,
    #[serde(rename = "ServerEphemeral", alias = "server_ephemeral")]
    pub server_ephemeral: String,
    #[serde(rename = "Salt", alias = "salt")]
    pub salt: String,
    #[serde(rename = "SRPSession", alias = "srp_session_id")]
    pub srp_session_id: String,
    #[serde(flatten)]
    pub response: ApiResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthenticationResponse {
    #[serde(rename = "UID", alias = "session_id")]
    pub session_id: SessionId,
    #[serde(rename = "UserID", alias = "user_id")]
    pub user_id: UserId,
    #[serde(rename = "EventID", alias = "event_id")]
    pub event_id: Option<EventId>,
    #[serde(rename = "AccessToken", alias = "access_token", default)]
    pub access_token: Option<String>,
    #[serde(rename = "RefreshToken", alias = "refresh_token", default)]
    pub refresh_token: Option<String>,
    #[serde(rename = "Scopes", alias = "scopes", default)]
    pub scopes: Vec<String>,
    #[serde(rename = "PasswordMode", alias = "password_mode", default)]
    pub password_mode: Option<PasswordMode>,
    #[serde(rename = "2FA", alias = "two_factor", default)]
    pub second_factor_parameters: Option<SecondFactorParameters>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecondFactorParameters {
    #[serde(
        rename = "Enabled",
        alias = "enabled",
        default,
        deserialize_with = "deserialize_boolish"
    )]
    pub is_enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RefreshSessionResponse {
    #[serde(rename = "AccessToken", alias = "access_token")]
    pub access_token: String,
    #[serde(rename = "RefreshToken", alias = "refresh_token")]
    pub refresh_token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScopesResponse {
    #[serde(rename = "Scopes", alias = "scopes", default)]
    pub scopes: Vec<String>,
    #[serde(
        rename = "TwoFactor",
        alias = "two_factor",
        default,
        deserialize_with = "deserialize_option_boolish"
    )]
    pub is_waiting_for_second_factor_code: Option<bool>,
    #[serde(flatten)]
    pub response: ApiResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModulusResponse {
    #[serde(rename = "Modulus", alias = "modulus")]
    pub modulus: String,
    #[serde(flatten)]
    pub response: ApiResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddressPublicKeyListResponse {
    #[serde(rename = "Address", alias = "address", default)]
    pub address: Option<String>,
    #[serde(rename = "AddressPublicKeys", alias = "address_public_keys", default)]
    pub address_public_keys: Vec<AddressPublicKey>,
    #[serde(flatten)]
    pub response: ApiResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddressPublicKey {
    #[serde(rename = "Email", alias = "email", default)]
    pub email: Option<String>,
    #[serde(rename = "Flags", alias = "flags", default)]
    pub flags: Option<i32>,
    #[serde(rename = "PublicKey", alias = "public_key", default)]
    pub public_key: Option<String>,
    #[serde(rename = "Source", alias = "source", default)]
    pub source: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeySaltListResponse {
    #[serde(rename = "KeySalts", alias = "key_salts", default)]
    pub key_salts: Vec<KeySalt>,
    #[serde(flatten)]
    pub response: ApiResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeySalt {
    #[serde(rename = "ID", alias = "key_id")]
    pub key_id: String,
    #[serde(
        rename = "KeySalt",
        alias = "key_salt",
        deserialize_with = "deserialize_binary_bytes"
    )]
    pub value: Vec<u8>,
}