use reqwest::StatusCode;
use serde::Deserialize;
use serde::de::Error as _;

use crate::api::ApiResponse;
use crate::auth::TokenCredential;

#[async_trait::async_trait]
pub trait UsersApiClient: Send + Sync {
    async fn get_user(&self) -> anyhow::Result<UserResponse>;
}

pub struct DefaultUsersApiClient {
    http_client: reqwest::Client,
    token_credential: Option<TokenCredential>,
}

impl DefaultUsersApiClient {
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
impl UsersApiClient for DefaultUsersApiClient {
    async fn get_user(&self) -> anyhow::Result<UserResponse> {
        let endpoint = Self::endpoint("core/v4/users")?;

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
        log::debug!("body: {:#?}", body);
        Ok(serde_json::from_slice::<UserResponse>(&body)?)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserResponse {
    #[serde(rename = "User", alias = "user")]
    pub user: Option<UserDto>,
    #[serde(flatten)]
    pub response: ApiResponse,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserDto {
    #[serde(rename = "Keys", alias = "keys")]
    pub keys: Vec<UserKeyDto>,
    
    #[serde(rename = "MaxSpace", alias = "max_space", default)]
    pub max_space: i64,
    
    #[serde(rename = "UsedSpace", alias = "used_space", default)]
    pub used_space: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserKeyDto {
    #[serde(rename = "ID", alias = "id")]
    pub id: String,
    #[serde(rename = "PrivateKey", alias = "private_key")]
    pub private_key: String,
    #[serde(
        rename = "Active",
        alias = "active",
        default,
        deserialize_with = "deserialize_boolish"
    )]
    pub is_active: bool,
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
