use reqwest::StatusCode;
use serde::Deserialize;
use serde::de::Error as _;

use crate::api::ApiResponse;
use crate::auth::TokenCredential;

#[async_trait::async_trait]
pub trait AddressesApiClient: Send + Sync {
    async fn get_addresses(&self) -> anyhow::Result<AddressesResponse>;

    async fn get_address(&self, address_id: &str) -> anyhow::Result<AddressResponse>;
}

pub struct DefaultAddressesApiClient {
    http_client: reqwest::Client,
    token_credential: Option<TokenCredential>,
}

impl DefaultAddressesApiClient {
    const DEFAULT_API_BASE: &'static str = "https://drive-api.proton.me/";

    pub fn new(http_client: reqwest::Client) -> Self {
        Self {
            http_client,
            token_credential: None,
        }
    }

    pub fn new_with_token_credential(
        http_client: reqwest::Client,
        token_credential: TokenCredential,
    ) -> Self {
        Self {
            http_client,
            token_credential: Some(token_credential),
        }
    }

    fn endpoint(path: &str) -> anyhow::Result<String> {
        let base = reqwest::Url::parse(Self::DEFAULT_API_BASE)?;
        Ok(base.join(path)?.to_string())
    }
}

#[async_trait::async_trait]
impl AddressesApiClient for DefaultAddressesApiClient {
    async fn get_addresses(&self) -> anyhow::Result<AddressesResponse> {
        let endpoint = Self::endpoint("core/v4/addresses")?;

        let response = if let Some(token_credential) = &self.token_credential {
            let (access_token, _) = token_credential.get_tokens().await?;
            let session_id = token_credential.session_id();

            let first = self
                .http_client
                .get(endpoint.clone())
                .bearer_auth(access_token.clone())
                .header("x-pm-uid", session_id.raw())
                .send()
                .await?;

            if first.status() == StatusCode::UNAUTHORIZED {
                let refreshed = token_credential
                    .get_refreshed_access_token(access_token)
                    .await?;
                self.http_client
                    .get(endpoint)
                    .bearer_auth(refreshed)
                    .header("x-pm-uid", session_id.raw())
                    .send()
                    .await?
                    .error_for_status()?
            } else {
                first.error_for_status()?
            }
        } else {
            self.http_client
                .get(endpoint)
                .send()
                .await?
                .error_for_status()?
        };

        let body = response.bytes().await?;
        Ok(serde_json::from_slice::<AddressesResponse>(&body)?)
    }

    async fn get_address(&self, address_id: &str) -> anyhow::Result<AddressResponse> {
        let endpoint = Self::endpoint(format!("core/v4/addresses/{address_id}").as_str())?;

        let response = if let Some(token_credential) = &self.token_credential {
            let (access_token, _) = token_credential.get_tokens().await?;
            let session_id = token_credential.session_id();

            let first = self
                .http_client
                .get(endpoint.clone())
                .bearer_auth(access_token.clone())
                .header("x-pm-uid", session_id.raw())
                .send()
                .await?;

            if first.status() == StatusCode::UNAUTHORIZED {
                let refreshed = token_credential
                    .get_refreshed_access_token(access_token)
                    .await?;
                self.http_client
                    .get(endpoint)
                    .bearer_auth(refreshed)
                    .header("x-pm-uid", session_id.raw())
                    .send()
                    .await?
                    .error_for_status()?
            } else {
                first.error_for_status()?
            }
        } else {
            self.http_client
                .get(endpoint)
                .send()
                .await?
                .error_for_status()?
        };

        let body = response.bytes().await?;
        Ok(serde_json::from_slice::<AddressResponse>(&body)?)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddressesResponse {
    #[serde(rename = "Addresses", alias = "addresses", default)]
    pub addresses: Vec<AddressDto>,
    #[serde(flatten)]
    pub response: ApiResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddressResponse {
    #[serde(rename = "Address", alias = "address")]
    pub address: AddressDto,
    #[serde(flatten)]
    pub response: ApiResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddressDto {
    #[serde(rename = "ID", alias = "id")]
    pub id: String,
    #[serde(rename = "Order", alias = "order", default)]
    pub order: i32,
    #[serde(rename = "Email", alias = "email", default)]
    pub email: String,
    #[serde(rename = "Status", alias = "status", default)]
    pub status: i32,
    #[serde(rename = "Keys", alias = "keys", default)]
    pub keys: Vec<AddressKeyDto>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddressKeyDto {
    #[serde(rename = "ID", alias = "id")]
    pub id: String,
    #[serde(rename = "PrivateKey", alias = "private_key", default)]
    pub private_key: String,
    #[serde(rename = "Token", alias = "token", default)]
    pub token: Option<String>,
    #[serde(rename = "Signature", alias = "signature", default)]
    pub signature: Option<String>,
    #[serde(
        rename = "Primary",
        alias = "primary",
        default,
        deserialize_with = "deserialize_boolish"
    )]
    pub is_primary: bool,
    #[serde(
        rename = "Active",
        alias = "active",
        default,
        deserialize_with = "deserialize_boolish"
    )]
    pub is_active: bool,
    #[serde(rename = "Flags", alias = "flags", default)]
    pub flags: i32,
}

use bitflags::bitflags;

bitflags! {
    #[derive(Debug, Clone, Copy)]
    pub struct AddressKeyFlags: i32 {
        const CAN_VERIFY  = 1;
        const CAN_ENCRYPT = 2;
    }
}

fn deserialize_boolish<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
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
        _ => Err(D::Error::custom(
            "expected bool/int/string for boolean field",
        )),
    }
}
