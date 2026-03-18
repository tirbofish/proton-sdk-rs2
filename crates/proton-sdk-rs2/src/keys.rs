use reqwest::StatusCode;

use crate::api::response::{AddressPublicKeyListResponse, KeySaltListResponse};
use crate::auth::TokenCredential;

#[async_trait::async_trait]
pub trait KeysApiClient: Send + Sync {
    async fn get_active_public_keys(
        &self,
        email_address: String,
    ) -> anyhow::Result<AddressPublicKeyListResponse>;

    async fn get_key_salts(&self) -> anyhow::Result<KeySaltListResponse>;
}

pub struct DefaultKeysApiClient {
    http_client: reqwest::Client,
    token_credential: Option<TokenCredential>,
}

impl DefaultKeysApiClient {
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
        if path.starts_with("http://") || path.starts_with("https://") {
            return Ok(path.to_string());
        }

        let base = reqwest::Url::parse(Self::DEFAULT_API_BASE)?;
        Ok(base.join(path)?.to_string())
    }
}

#[async_trait::async_trait]
impl KeysApiClient for DefaultKeysApiClient {
    async fn get_active_public_keys(
        &self,
        email_address: String,
    ) -> anyhow::Result<AddressPublicKeyListResponse> {
        let mut url = reqwest::Url::parse(&Self::endpoint("core/v4/keys/all")?)?;
        url.query_pairs_mut()
            .append_pair("InternalOnly", "1")
            .append_pair("Email", &email_address);

        let response = if let Some(token_credential) = &self.token_credential {
            let (access_token, _) = token_credential.get_tokens().await?;
            let session_id = token_credential.session_id();

            let first = self
                .http_client
                .get(url.clone())
                .bearer_auth(access_token.clone())
                .header("x-pm-uid", session_id.raw())
                .send()
                .await?;

            if first.status() == StatusCode::UNAUTHORIZED {
                let refreshed = token_credential
                    .get_refreshed_access_token(access_token)
                    .await?;
                self.http_client
                    .get(url)
                    .bearer_auth(refreshed)
                    .header("x-pm-uid", session_id.raw())
                    .send()
                    .await?
                    .error_for_status()?
            } else {
                first.error_for_status()?
            }
        } else {
            self.http_client.get(url).send().await?.error_for_status()?
        };
        crate::utils::decode_json::<AddressPublicKeyListResponse>(response).await
    }

    async fn get_key_salts(&self) -> anyhow::Result<KeySaltListResponse> {
        let endpoint = Self::endpoint("core/v4/keys/salts")?;

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
        crate::utils::decode_json::<KeySaltListResponse>(response).await
    }
}
