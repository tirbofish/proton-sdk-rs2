use crate::api::ApiResponse;
use crate::error::TooManyRequestsException;
use async_trait::async_trait;
use bytes::Bytes;
use proton_sdk_rs2::auth::TokenCredential;
use reqwest_middleware::ClientWithMiddleware;
use tokio::time::sleep;

const MAX_RETRIES: u32 = 3;

#[async_trait]
pub trait StorageApiClient: Send + Sync {
    async fn upload_blob(
        &self,
        base_url: &str,
        token: &str,
        data: bytes::Bytes,
    ) -> anyhow::Result<ApiResponse>;

    async fn get_blob_stream(
        &self,
        base_url: &str,
        token: &str,
    ) -> anyhow::Result<reqwest::Response>;
}

pub struct DefaultStorageApiClient {
    #[allow(dead_code)]
    http_client: ClientWithMiddleware,
    storage_client: ClientWithMiddleware,
    _default_api_base_url: reqwest::Url,
    _storage_api_base_url: reqwest::Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultStorageApiClient {
    pub fn new(
        http_client: ClientWithMiddleware,
        storage_client: ClientWithMiddleware,
        default_api_base_url: reqwest::Url,
        storage_api_base_url: reqwest::Url,
        token_credential: Option<TokenCredential>,
    ) -> Self {
        Self {
            http_client,
            storage_client,
            _default_api_base_url: default_api_base_url,
            _storage_api_base_url: storage_api_base_url,
            token_credential,
        }
    }

    async fn add_auth_headers(
        &self,
        mut builder: reqwest_middleware::RequestBuilder,
    ) -> anyhow::Result<reqwest_middleware::RequestBuilder> {
        if let Some(credential) = &self.token_credential {
            let (access_token, _) = credential.get_tokens().await?;
            builder = builder.header("Authorization", format!("Bearer {}", access_token));
            builder = builder.header("x-pm-uid", credential.session_id().raw());
        }
        Ok(builder)
    }
}

#[async_trait]
impl StorageApiClient for DefaultStorageApiClient {
    async fn upload_blob(
        &self,
        base_url: &str,
        token: &str,
        data: Bytes,
    ) -> anyhow::Result<ApiResponse> {
        let mut last_err = None;

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = crate::error::retry_backoff_delay(attempt);
                sleep(delay).await;
            }

            let blob_part = reqwest::multipart::Part::bytes(Vec::<u8>::from(data.clone()))
                .file_name("blob")
                .mime_str("application/octet-stream")?;

            let form = reqwest::multipart::Form::new().part("Block", blob_part);

            let builder = self
                .storage_client
                .post(base_url)
                .header("pm-storage-token", token)
                .multipart(form);

            let builder = self.add_auth_headers(builder).await?;

            match builder.send().await {
                Ok(response) => {
                    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        last_err = Some(
                            TooManyRequestsException::from_headers(response.headers()).into(),
                        );
                        if let Some(wait) = crate::error::parse_retry_after(response.headers()) {
                            sleep(wait).await;
                        }
                        continue;
                    }
                    if response.status().is_server_error() {
                        last_err = Some(anyhow::anyhow!(
                            "server error on attempt {}: {}",
                            attempt + 1,
                            response.status()
                        ));
                        continue;
                    }
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    // Proton's blob storage backend returns an empty body (or
                    // occasionally plain text) on a successful upload — not JSON.
                    // Treat any 2xx with a non-JSON body as success.
                    if body.trim().is_empty() || !body.trim_start().starts_with('{') {
                        if status.is_success() {
                            return Ok(ApiResponse {
                                code: crate::api::ResponseCode(1000),
                                error_message: None,
                            });
                        } else {
                            last_err = Some(anyhow::anyhow!(
                                "blob upload failed: status {}, body: {}",
                                status,
                                body
                            ));
                            continue;
                        }
                    }
                    return serde_json::from_str::<ApiResponse>(&body).map_err(|e| {
                        anyhow::anyhow!("error decoding response body: {}. Body: {}", e, body)
                    });
                }
                Err(e) => {
                    // retry on network errors
                    if e.is_connect() || e.is_timeout() {
                        last_err = Some(anyhow::anyhow!(
                            "network error on attempt {}: {}",
                            attempt + 1,
                            e
                        ));
                        continue;
                    }
                    return Err(e.into());
                }
            }
        }

        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("upload failed after {} attempts", MAX_RETRIES + 1)))
    }

    async fn get_blob_stream(
        &self,
        bare_url: &str,
        token: &str,
    ) -> anyhow::Result<reqwest::Response> {
        let builder = self
            .storage_client
            .get(bare_url)
            .header("pm-storage-token", token)
            // Disable automatic decompression - the blob is already encrypted,
            // not compressed, but some servers/proxies may send misleading
            // Content-Encoding headers causing reqwest to fail decoding
            .header("Accept-Encoding", "identity");

        let builder = self.add_auth_headers(builder).await?;

        let response = builder.send().await?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(TooManyRequestsException::from_headers(response.headers()).into());
        }

        if !response.status().is_success() {
            anyhow::bail!("blob download failed with status {}", response.status());
        }

        Ok(response)
    }
}
