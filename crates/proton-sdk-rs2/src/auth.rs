use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, Weak},
};

use http::Uri;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Serialize;
use tokio::sync::{OnceCell, RwLock, broadcast};

use crate::{
    SessionId,
    api::{
        ApiResponse,
        response::{
            AuthenticationResponse, ModulusResponse, RefreshSessionResponse, ScopesResponse,
            SesisonInitiationResponse,
        },
    },
};

#[derive(Clone)]
pub struct TokenCredential {
    session_id: SessionId,
    #[allow(dead_code)]
    access_token: String,
    #[allow(dead_code)]
    refresh_token: String,
    state: Arc<TokenCredentialState>,
}

struct TokenCredentialState {
    client: Arc<dyn AuthenticationApiClient>,
    tokens_task: RwLock<Arc<OnceCell<(String, String)>>>,
    tokens_refreshed_tx: broadcast::Sender<(String, String)>,
    refresh_token_expired_tx: broadcast::Sender<()>,
}

static TOKEN_STATES: OnceLock<Mutex<HashMap<String, Weak<TokenCredentialState>>>> = OnceLock::new();

impl TokenCredential {
    /// Creates a new `TokenCredential` seeded with the given access and refresh tokens.
    /// The underlying token pair is refreshed automatically when callers invoke `get_tokens`.
    pub fn new(
        client: Arc<dyn AuthenticationApiClient>,
        session_id: SessionId,
        access_token: String,
        refresh_token: String,
    ) -> Self {
        let registry = TOKEN_STATES.get_or_init(|| Mutex::new(HashMap::new()));
        let state = {
            let mut registry = registry.lock().unwrap();
            if let Some(state) = registry
                .get(session_id.raw())
                .and_then(|state| state.upgrade())
            {
                state
            } else {
                let tokens_task = OnceCell::new();
                let _ = tokens_task.set((access_token.clone(), refresh_token.clone()));

                let (tokens_refreshed_tx, _) = broadcast::channel(16);
                let (refresh_token_expired_tx, _) = broadcast::channel(16);
                let state = Arc::new(TokenCredentialState {
                    client,
                    tokens_task: RwLock::new(Arc::new(tokens_task)),
                    tokens_refreshed_tx,
                    refresh_token_expired_tx,
                });
                registry.insert(session_id.raw().clone(), Arc::downgrade(&state));
                state
            }
        };

        Self {
            session_id,
            state,
            access_token,
            refresh_token,
        }
    }

    /// Returns the current valid (access, refresh) token pair, refreshing silently if needed.
    pub async fn get_tokens(&self) -> anyhow::Result<(String, String)> {
        let task = self.state.tokens_task.read().await.clone();
        let tokens = task
            .get()
            .ok_or_else(|| anyhow::anyhow!("Tokens not initialized"))?;
        Ok((tokens.0.clone(), tokens.1.clone()))
    }

    /// Alias for `get_tokens`; returns the (access, refresh) token pair.
    pub async fn get_access_token(&self) -> anyhow::Result<(String, String)> {
        self.get_tokens().await
    }

    /// Subscribes to a broadcast channel that fires whenever tokens are successfully refreshed.
    /// The sent value is the new (access, refresh) token pair.
    pub fn subscribe_tokens_refreshed(&self) -> broadcast::Receiver<(String, String)> {
        self.state.tokens_refreshed_tx.subscribe()
    }

    /// Subscribes to a broadcast channel that fires when the refresh token is irrevocably expired.
    /// Callers should prompt for re-authentication on receipt.
    pub fn subscribe_refresh_token_expired(&self) -> broadcast::Receiver<()> {
        self.state.refresh_token_expired_tx.subscribe()
    }

    /// Returns the session identifier associated with this credential.
    pub fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }

    /// Returns the access token that was last explicitly stored in this credential.
    /// For the latest valid token use the async `get_tokens` instead.
    pub fn current_access_token(&self) -> &str {
        &self.access_token
    }

    /// Returns the refresh token that was active when this credential was constructed or last
    /// explicitly stored.
    ///
    /// Note: if the access token has been silently refreshed during the session the internally
    /// stored refresh token field is not updated; callers that need the latest token pair should
    /// use the async [`Self::get_tokens`] instead.
    pub fn current_refresh_token(&self) -> &str {
        &self.refresh_token
    }

    fn trigger_tokens_refreshed(&self, access_token: String, refresh_token: String) {
        let _ = self
            .state
            .tokens_refreshed_tx
            .send((access_token, refresh_token));
    }

    #[allow(dead_code)]
    fn trigger_refresh_token_expired(&self) {
        let _ = self.state.refresh_token_expired_tx.send(());
    }

    /// Obtains a fresh access token, triggering a server-side refresh if the given
    /// `rejected_access_token` matches the current stored token.
    pub async fn get_refreshed_access_token(
        &self,
        rejected_access_token: String,
    ) -> anyhow::Result<String> {
        let current_tokens_task = self.state.tokens_task.read().await.clone();

        let (current_access_token, current_refresh_token) = {
            let tokens = current_tokens_task
                .get()
                .ok_or_else(|| anyhow::anyhow!("Tokens not initialized"))?;
            (tokens.0.clone(), tokens.1.clone())
        };

        let is_likely_already_refreshed = current_access_token != rejected_access_token;
        if is_likely_already_refreshed {
            return Ok(current_access_token);
        }

        let refreshed_tokens_task = Arc::new(OnceCell::new());
        let mut tokens_task_guard = self.state.tokens_task.write().await;
        let tokens_task_replaced = Arc::ptr_eq(&*tokens_task_guard, &current_tokens_task);

        let selected_tokens_task = if tokens_task_replaced {
            *tokens_task_guard = refreshed_tokens_task.clone();
            refreshed_tokens_task
        } else {
            tokens_task_guard.clone()
        };
        drop(tokens_task_guard);

        let client = self.state.client.clone();
        let session_id = self.session_id.clone();
        let current_access = current_access_token.clone();
        let current_refresh = current_refresh_token.clone();

        let (access_token, refresh_token) = selected_tokens_task
            .get_or_init(|| async move {
                let result = async {
                    let response = client
                        .refresh_session(
                            session_id,
                            current_access.clone(),
                            current_refresh.clone(),
                        )
                        .await?;
                    Ok::<_, anyhow::Error>((response.access_token, response.refresh_token))
                }
                .await;

                match result {
                    Ok(tokens) => tokens,
                    Err(_) => (current_access, current_refresh),
                }
            })
            .await
            .clone();

        if tokens_task_replaced {
            self.trigger_tokens_refreshed(access_token.clone(), refresh_token);
        }

        Ok(access_token)
    }
}

#[async_trait::async_trait]
pub trait AuthenticationApiClient: Send + Sync {
    async fn initiate_session(&self, username: String)
    -> anyhow::Result<SesisonInitiationResponse>;

    async fn authenticate(
        &self,
        initiation_response: SesisonInitiationResponse,
        srp_client_handshake: proton_crypto::srp::ClientProof,
        username: String,
    ) -> anyhow::Result<AuthenticationResponse>;

    async fn validate_second_factor(
        &self,
        second_factor_code: String,
    ) -> anyhow::Result<ScopesResponse>;

    async fn end_session(&self) -> anyhow::Result<ApiResponse>;

    async fn end_session_with_token(
        &self,
        session_id: SessionId,
        access_token: String,
    ) -> anyhow::Result<ApiResponse>;

    async fn refresh_session(
        &self,
        session_id: SessionId,
        access_token: String,
        refresh_token: String,
    ) -> anyhow::Result<RefreshSessionResponse>;

    async fn get_scopes(&self) -> anyhow::Result<ScopesResponse>;

    async fn get_random_srp_modulus(&self) -> anyhow::Result<ModulusResponse>;
}

pub struct DefaultAuthenticationApiClient {
    http_client: reqwest::Client,
    refresh_redirect_uri: Uri,
}

impl DefaultAuthenticationApiClient {
    const DEFAULT_API_BASE: &'static str = "https://drive-api.proton.me/";

    pub fn new(http_client: reqwest::Client, refresh_redirect_uri: Uri) -> Self {
        Self {
            http_client,
            refresh_redirect_uri,
        }
    }

    fn endpoint(&self, path: &str) -> anyhow::Result<String> {
        if path.starts_with("http://") || path.starts_with("https://") {
            return Ok(path.to_string());
        }

        let base = reqwest::Url::parse(Self::DEFAULT_API_BASE)?;
        Ok(base.join(path)?.to_string())
    }

    fn auth_headers(session_id: &SessionId, access_token: &str) -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        let bearer = format!("Bearer {}", access_token);
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&bearer)?);
        headers.insert("x-pm-uid", HeaderValue::from_str(session_id.raw())?);
        Ok(headers)
    }
}

#[derive(Serialize)]
struct SessionInitiationRequest {
    #[serde(rename = "Username")]
    username: String,
}

#[derive(Serialize)]
struct AuthenticationRequest {
    #[serde(rename = "ClientEphemeral")]
    client_ephemeral: String,
    #[serde(rename = "ClientProof")]
    client_proof: String,
    #[serde(rename = "SRPSession")]
    srp_session_id: String,
    #[serde(rename = "Username")]
    username: String,
}

#[derive(Serialize)]
struct SecondFactorValidationRequest {
    #[serde(rename = "TwoFactorCode")]
    second_factor_code: String,
}

#[derive(Serialize)]
struct SessionRefreshRequest {
    #[serde(rename = "RefreshToken")]
    refresh_token: String,
    #[serde(rename = "ResponseType")]
    response_type: String,
    #[serde(rename = "GrantType")]
    grant_type: String,
    #[serde(rename = "RedirectURI")]
    redirect_uri: String,
}

#[async_trait::async_trait]
impl AuthenticationApiClient for DefaultAuthenticationApiClient {
    async fn initiate_session(
        &self,
        username: String,
    ) -> anyhow::Result<SesisonInitiationResponse> {
        let request = SessionInitiationRequest { username };
        let request_body = serde_json::to_vec(&request)?;

        let response = self
            .http_client
            .post(self.endpoint("auth/v4/info")?)
            .header("content-type", "application/json")
            .body(request_body)
            .send()
            .await?
            .error_for_status()?;

        crate::utils::decode_json::<SesisonInitiationResponse>(response).await
    }

    async fn authenticate(
        &self,
        initiation_response: SesisonInitiationResponse,
        srp_client_handshake: proton_crypto::srp::ClientProof,
        username: String,
    ) -> anyhow::Result<AuthenticationResponse> {
        let request = AuthenticationRequest {
            client_ephemeral: srp_client_handshake.ephemeral.clone(),
            client_proof: srp_client_handshake.proof.clone(),
            srp_session_id: initiation_response.srp_session_id,
            username,
        };
        let request_body = serde_json::to_vec(&request)?;

        let response = self
            .http_client
            .post(self.endpoint("auth/v4")?)
            .header("content-type", "application/json")
            .body(request_body)
            .send()
            .await?
            .error_for_status()?;

        crate::utils::decode_json::<AuthenticationResponse>(response).await
    }

    async fn validate_second_factor(
        &self,
        second_factor_code: String,
    ) -> anyhow::Result<ScopesResponse> {
        let request = SecondFactorValidationRequest { second_factor_code };
        let request_body = serde_json::to_vec(&request)?;

        let response = self
            .http_client
            .post(self.endpoint("auth/v4/2fa")?)
            .header("content-type", "application/json")
            .body(request_body)
            .send()
            .await?
            .error_for_status()?;

        crate::utils::decode_json::<ScopesResponse>(response).await
    }

    async fn end_session(&self) -> anyhow::Result<ApiResponse> {
        let response = self
            .http_client
            .delete(self.endpoint("auth/v4")?)
            .send()
            .await?
            .error_for_status()?;

        crate::utils::decode_json::<ApiResponse>(response).await
    }

    async fn end_session_with_token(
        &self,
        session_id: SessionId,
        access_token: String,
    ) -> anyhow::Result<ApiResponse> {
        let headers = Self::auth_headers(&session_id, &access_token)?;
        let response = self
            .http_client
            .delete(self.endpoint("auth/v4")?)
            .headers(headers)
            .send()
            .await?
            .error_for_status()?;

        crate::utils::decode_json::<ApiResponse>(response).await
    }

    async fn refresh_session(
        &self,
        session_id: SessionId,
        access_token: String,
        refresh_token: String,
    ) -> anyhow::Result<RefreshSessionResponse> {
        let request = SessionRefreshRequest {
            refresh_token,
            response_type: "token".to_string(),
            grant_type: "refresh_token".to_string(),
            redirect_uri: self.refresh_redirect_uri.to_string(),
        };

        let headers = Self::auth_headers(&session_id, &access_token)?;

        let response = self
            .http_client
            .post(self.endpoint("auth/v4/refresh")?)
            .headers(headers)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&request)?)
            .send()
            .await?
            .error_for_status()?;

        crate::utils::decode_json::<RefreshSessionResponse>(response).await
    }

    async fn get_scopes(&self) -> anyhow::Result<ScopesResponse> {
        let response = self
            .http_client
            .get(self.endpoint("auth/v4/scopes")?)
            .send()
            .await?
            .error_for_status()?;

        crate::utils::decode_json::<ScopesResponse>(response).await
    }

    async fn get_random_srp_modulus(&self) -> anyhow::Result<ModulusResponse> {
        let response = self
            .http_client
            .get(self.endpoint("auth/v4/modulus")?)
            .send()
            .await?
            .error_for_status()?;

        crate::utils::decode_json::<ModulusResponse>(response).await
    }
}
