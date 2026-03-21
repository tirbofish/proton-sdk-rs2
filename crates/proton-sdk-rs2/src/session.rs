use std::{fmt::Display, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose};
use proton_srp::{RPGPVerifier, SRPAuth, SrpHashVersion};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

use crate::auth::DefaultAuthenticationApiClient;
use crate::keys::{DefaultKeysApiClient, KeysApiClient};
use crate::secret::SessionSecretCache;
use crate::{
    PasswordMode, SessionId, UserId,
    auth::{AuthenticationApiClient, TokenCredential},
    cache::CacheRepository,
    client::{ApiClient, ProtonClientConfiguration, ProtonClientOptions},
    secret::DefaultSecretCache,
};

/// Stored the tokens and relavant information related to session management for any Proton-API
/// based app.
pub struct ProtonAPISession {
    /// The session id generated through [`proton_srp`].
    pub session_id: SessionId,
    /// The username of this authenticated user.
    pub username: String,
    /// The userid.
    pub user_id: UserId,
    /// The refresh token and access token required to access any api function
    pub token_credential: TokenCredential,
    /// Scopes. I'm not sure what they do...
    pub scopes: Vec<String>,
    /// Checks if the session requires a second factor code.
    ///
    /// You can apply the 2FA code with [`Self::apply_second_factor_code`]
    pub is_waiting_for_second_factor_code: bool,
    /// The password mode
    pub password_mode: PasswordMode,
    /// The configuration of the client.
    pub client_config: ProtonClientConfiguration,
    /// States if the session has ended.
    /// Potentially through:
    /// - session cancellation from the web client
    /// - logout through [`Self::end_from_session`]
    pub is_ended: bool,

    pub(crate) session_secret_cache: Arc<dyn SessionSecretCache>,

    keys_api: Option<Arc<dyn KeysApiClient>>,
    authentication_api: Option<Arc<dyn AuthenticationApiClient>>,

    pub http_client: reqwest::Client,
}

impl Display for ProtonAPISession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ProtonAPISession {{ session_id: {}, username: {}, user_id: {}, scopes: {:?}, is_waiting_for_second_factor_code: {}, password_mode: {:?}, is_ended: {} }}",
            self.session_id.raw(),
            self.username,
            self.user_id.raw(),
            self.scopes,
            self.is_waiting_for_second_factor_code,
            self.password_mode,
            self.is_ended,
        )
    }
}

impl ProtonAPISession {
    pub(crate) fn new(
        session_id: SessionId,
        username: String,
        user_id: UserId,
        token_credential: TokenCredential,
        scopes: Vec<String>,
        is_waiting_for_second_factor_code: bool,
        password_mode: PasswordMode,
        client_config: ProtonClientConfiguration,
    ) -> Self {
        let http_client = Self::create_http_client(
            &client_config,
            Some((&session_id, token_credential.current_access_token())),
        )
        .expect("Failed to create session HTTP client");
        let secret_cache = DefaultSecretCache::new(client_config.secret_cache_repository.clone());

        Self {
            session_id,
            username,
            user_id,
            token_credential,
            scopes,
            is_waiting_for_second_factor_code,
            password_mode,
            client_config,
            session_secret_cache: Arc::new(secret_cache),
            keys_api: None,
            is_ended: false,
            authentication_api: None,
            http_client,
        }
    }

    fn create_http_client(
        config: &ProtonClientConfiguration,
        auth: Option<(&SessionId, &str)>,
    ) -> anyhow::Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder();
        let mut default_headers = HeaderMap::new();

        let app_version = ProtonClientConfiguration::build_pm_app_version(config);
        log::debug!("Generated x-pm-appversion header for session: {}", app_version);
        default_headers.insert("x-pm-appversion", HeaderValue::from_str(&app_version)?);

        if let Some((session_id, access_token)) = auth {
            let bearer = format!("Bearer {access_token}");
            default_headers.insert(AUTHORIZATION, HeaderValue::from_str(&bearer)?);
            default_headers.insert("x-pm-uid", HeaderValue::from_str(session_id.raw())?);
        }

        builder = builder.default_headers(default_headers);

        if !config.user_agent.is_empty() {
            builder = builder.user_agent(config.user_agent.clone());
        }

        Ok(builder.build()?)
    }

    fn create_authentication_api_client(
        config: &ProtonClientConfiguration,
    ) -> anyhow::Result<Arc<dyn AuthenticationApiClient>> {
        let http_client = Self::create_http_client(config, None)?;
        Ok(Arc::new(ApiClient::create_authentication_api_client(
            http_client,
            config.refresh_redirect_uri.clone(),
        )))
    }

    pub async fn begin(
        username: impl Into<String>,
        password: &str,
        app_version: semver::Version,
        mut session_options: ProtonSessionOptions,
    ) -> anyhow::Result<ProtonAPISession> {
        let username: String = username.into();
        if let Some(secret_repo) = session_options.secret_cache_repository.take() {
            session_options.client.secret_cache_repository = Some(secret_repo);
        }

        let configuration = ProtonClientConfiguration::new(app_version, session_options.client)?;
        let auth_api_client = Self::create_authentication_api_client(&configuration)?;
        let session_init_resp = auth_api_client.initiate_session(username.clone()).await?;

        log::debug!(
            "SRP session {} initialised",
            session_init_resp.srp_session_id
        );

        let client = SRPAuth::new(
            &RPGPVerifier::default(),
            Some(&username),
            password,
            SrpHashVersion::V4,
            &session_init_resp.salt,
            &session_init_resp.modulus,
            &session_init_resp.server_ephemeral,
        )?;

        let proof = client.generate_proofs()?;
        let client_proof = proton_crypto::srp::ClientProof {
            ephemeral: general_purpose::STANDARD.encode(proof.client_ephemeral),
            proof: general_purpose::STANDARD.encode(proof.client_proof),
            expected_server_proof: general_purpose::STANDARD.encode(proof.expected_server_proof),
        };

        let auth_resp = auth_api_client
            .authenticate(session_init_resp, client_proof, username.clone())
            .await?;

        let access_token = auth_resp.access_token.clone().ok_or_else(|| {
            anyhow::anyhow!("AuthenticationResponse.AccessToken is not yet modeled")
        })?;
        let refresh_token = auth_resp.refresh_token.clone().ok_or_else(|| {
            anyhow::anyhow!("AuthenticationResponse.RefreshToken is not yet modeled")
        })?;

        let token_credential = TokenCredential::new(
            auth_api_client,
            auth_resp.session_id.clone(),
            access_token,
            refresh_token,
        );

        let password_mode = auth_resp.password_mode.unwrap_or(PasswordMode::Single);
        let mut session = ProtonAPISession::new(
            auth_resp.session_id,
            username,
            auth_resp.user_id,
            token_credential,
            auth_resp.scopes,
            auth_resp
                .second_factor_parameters
                .as_ref()
                .map(|p| p.is_enabled)
                .unwrap_or(false),
            password_mode,
            configuration,
        );
        //  && matches!(session.password_mode, PasswordMode::Single)

        if !session.is_waiting_for_second_factor_code {
            if let Err(error) = session.apply_data_password(password).await {
                log::warn!("Failed to apply data password: {error}");
            }
        }

        Ok(session)
    }

    pub fn resume(
        session_id: SessionId,
        username: impl Into<String>,
        user_id: UserId,
        access_token: String,
        refresh_token: String,
        scopes: Vec<String>,
        is_waiting_for_second_factor_code: bool,
        password_mode: PasswordMode,
        app_version: semver::Version,
        secret_cache_repository: Arc<dyn CacheRepository>,
    ) -> ProtonAPISession {
        ProtonAPISession::resume_with_options(
            session_id,
            username,
            user_id,
            access_token,
            refresh_token,
            scopes,
            is_waiting_for_second_factor_code,
            password_mode,
            app_version,
            secret_cache_repository,
            ProtonClientOptions::default(),
        )
    }

    pub fn resume_with_options(
        session_id: SessionId,
        username: impl Into<String>,
        user_id: UserId,
        access_token: String,
        refresh_token: String,
        scopes: Vec<String>,
        is_waiting_for_second_factor_code: bool,
        password_mode: PasswordMode,
        app_version: semver::Version,
        secret_cache_repository: Arc<dyn CacheRepository>,
        mut options: ProtonClientOptions,
    ) -> ProtonAPISession {
        options.secret_cache_repository = Some(secret_cache_repository);
        let configuration = ProtonClientConfiguration::new(app_version, options)
            .expect("Failed to create ProtonClientConfiguration");
        let auth_api_client = Self::create_authentication_api_client(&configuration)
            .expect("Failed to create authentication API client");

        let token_credential = TokenCredential::new(
            auth_api_client,
            session_id.clone(),
            access_token,
            refresh_token,
        );

        let session = ProtonAPISession::new(
            session_id,
            username.into(),
            user_id,
            token_credential,
            scopes,
            is_waiting_for_second_factor_code,
            password_mode,
            configuration,
        );

        log::debug!("Session {} was resumed", session.session_id.raw());
        session
    }

    pub fn renew(
        expired_session: ProtonAPISession,
        session_id: SessionId,
        access_token: String,
        refresh_token: String,
        scopes: Vec<String>,
        is_waiting_for_second_factor_code: bool,
        password_mode: PasswordMode,
    ) -> ProtonAPISession {
        let username = expired_session.username;
        let user_id = expired_session.user_id;
        let configuration = expired_session.client_config;
        let auth_api_client = Self::create_authentication_api_client(&configuration)
            .expect("Failed to create authentication API client");

        let token_credential = TokenCredential::new(
            auth_api_client,
            session_id.clone(),
            access_token,
            refresh_token,
        );

        ProtonAPISession::new(
            session_id,
            username,
            user_id,
            token_credential,
            scopes,
            is_waiting_for_second_factor_code,
            password_mode,
            configuration,
        )
    }

    pub async fn end_from_token(
        id: String,
        access_token: String,
        app_version: semver::Version,
        options: Option<ProtonClientOptions>,
    ) -> anyhow::Result<()> {
        let configuration =
            ProtonClientConfiguration::new(app_version, options.unwrap_or_default())?;
        let auth_api_client = Self::create_authentication_api_client(&configuration)?;
        let _ = auth_api_client
            .end_session_with_token(SessionId::new(id), access_token)
            .await?;
        Ok(())
    }

    pub async fn apply_second_factor_code(
        &mut self,
        second_factor_code: String,
    ) -> anyhow::Result<()> {
        let response = self
            .authentication_api()?
            .validate_second_factor(second_factor_code)
            .await?;

        self.is_waiting_for_second_factor_code = false;
        self.scopes = response.scopes;
        Ok(())
    }

    pub async fn apply_data_password(&mut self, password: &str) -> anyhow::Result<()> {
        let response = self.keys_api()?.get_key_salts().await?;

        log::debug!("Key salts response: {} salts", response.key_salts.len());
        for salt in &response.key_salts {
            log::debug!(
                "Salt key_id: {:?}, value empty: {}",
                salt.key_id,
                salt.value.is_empty()
            );

            if salt.value.is_empty() {
                continue;
            }

            let passphrase = Self::derive_secret_from_password(password, &salt.value)?;

            self.session_secret_cache
                .set_account_key_passphrase(&salt.key_id, &passphrase)
                .await?;
        }
        Ok(())
    }

    pub async fn refresh_scopes(&mut self) -> anyhow::Result<()> {
        let auth_api_client = Self::create_authentication_api_client(&self.client_config)?;
        let scopes_response = auth_api_client.get_scopes().await?;
        self.scopes = scopes_response.scopes;
        Ok(())
    }

    pub async fn ensure_authenticated(&mut self) -> anyhow::Result<()> {
        let (probe_access_token, _) = self.token_credential.get_tokens().await?;
        let probe = Self::create_http_client(
            &self.client_config,
            Some((&self.session_id, probe_access_token.as_str())),
        )?;

        let probe_response = probe
            .get("https://drive-api.proton.me/auth/v4/scopes")
            .send()
            .await?;

        // Proton API may return HTTP 200 with Code 401 in JSON body for expired tokens
        let probe_status = probe_response.status();
        log::debug!("ensure_authenticated probe status: {}", probe_status);
        let needs_refresh = if probe_status == StatusCode::UNAUTHORIZED {
            true
        } else if probe_status.is_success() {
            let body_text = probe_response.text().await.unwrap_or_default();
            log::debug!("ensure_authenticated probe body: {}", body_text);
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body_text) {
                let code = json.get("Code").and_then(|c| c.as_u64()).unwrap_or(1000);
                code == 401
            } else {
                false
            }
        } else {
            return probe_response
                .error_for_status()
                .map(|_| ())
                .map_err(Into::into);
        };

        if !needs_refresh {
            return Ok(());
        }

        log::debug!("ensure_authenticated: token needs refresh");
        let (access_token, _) = self.token_credential.get_tokens().await?;
        let refreshed_access_token = self
            .token_credential
            .get_refreshed_access_token(access_token.clone())
            .await?;

        if refreshed_access_token == access_token {
            anyhow::bail!("Session expired: access token could not be refreshed. Please re-authenticate.");
        }

        self.http_client = Self::create_http_client(
            &self.client_config,
            Some((&self.session_id, refreshed_access_token.as_str())),
        )?;
        self.keys_api = None;
        self.authentication_api = None;

        Ok(())
    }

    pub async fn end_from_session(&mut self) -> anyhow::Result<bool> {
        if self.is_ended {
            return Ok(true);
        }

        let auth_api_client = Self::create_authentication_api_client(&self.client_config)?;
        let _ = auth_api_client.end_session().await?;
        self.is_ended = true;
        Ok(true)
    }

    pub fn get_http_client(
        &self,
        base_route_path: Option<String>,
        attempt_timeout: Option<Duration>,
        total_timeout: Option<Duration>,
    ) -> anyhow::Result<reqwest::Client> {
        if base_route_path.is_none() && attempt_timeout.is_none() && total_timeout.is_none() {
            Ok(self.http_client.clone())
        } else {
            self.client_config.get_http_client(
                Some(self),
                base_route_path,
                attempt_timeout,
                total_timeout,
            )
        }
    }

    pub fn derive_secret_from_password(password: &str, salt: &[u8]) -> anyhow::Result<Vec<u8>> {
        let hash = proton_srp::mailbox_password_hash(password, salt)?;
        Ok(hash.as_bytes()[29..].to_vec())
    }

    #[allow(dead_code)]
    fn on_refresh_token_expired(&mut self) {
        self.is_ended = true;
    }
}

impl ProtonAPISession {
    pub fn keys_api(&mut self) -> anyhow::Result<Arc<dyn KeysApiClient>> {
        if let Some(api) = self.keys_api.as_ref() {
            Ok(api.clone())
        } else {
            let client = self.get_http_client(None, None, None)?;
            let api: Arc<dyn KeysApiClient> =
                Arc::new(DefaultKeysApiClient::new_with_token_credential(
                    client,
                    self.token_credential.clone(),
                ));
            self.keys_api = Some(Arc::clone(&api));
            Ok(api)
        }
    }

    pub fn authentication_api(&mut self) -> anyhow::Result<Arc<dyn AuthenticationApiClient>> {
        if let Some(api) = self.authentication_api.as_ref() {
            Ok(api.clone())
        } else {
            let client = self.get_http_client(None, None, None)?;
            let api = Arc::new(DefaultAuthenticationApiClient::new(
                client,
                self.client_config.refresh_redirect_uri.clone(),
            ));
            self.authentication_api = Some(api.clone());
            Ok(api)
        }
    }
}

pub struct ProtonSessionOptions {
    pub client: ProtonClientOptions,
    pub secret_cache_repository: Option<Arc<dyn CacheRepository>>,
}

impl ProtonSessionOptions {
    pub fn new(client_options: ProtonClientOptions) -> Self {
        let secret_cache_repository = client_options.secret_cache_repository.clone();
        Self {
            client: client_options,
            secret_cache_repository,
        }
    }
}
