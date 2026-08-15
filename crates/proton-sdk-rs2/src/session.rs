use std::{fmt::Display, sync::Arc, time::Duration};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rand::Rng;
use reqwest::StatusCode;
use zeroize::{Zeroize, Zeroizing};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};

use crate::auth::DefaultAuthenticationApiClient;
use crate::cache::InMemoryCacheRepository;
use crate::keys::{DefaultKeysApiClient, KeysApiClient};
use crate::secret::SessionSecretCache;
use crate::ser::StoredCredentials;
use crate::users::{DefaultUsersApiClient, UsersApiClient};
use crate::utils::AppVersionConfiguration;
use crate::{
    PasswordMode, SessionId, UserId,
    auth::{AuthenticationApiClient, TokenCredential},
    cache::CacheRepository,
    client::{ApiClient, ProtonApiDefaults, ProtonClientConfiguration, ProtonClientOptions},
    secret::DefaultSecretCache,
};

/// Stored the tokens and relavant information related to session management for any Proton-API
/// based app.
#[derive(Clone)]
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

        let app_version = config.app_version.to_string();
        log::debug!(
            "Generated x-pm-appversion header for session: {}",
            app_version
        );
        default_headers.insert("x-pm-appversion", HeaderValue::from_str(&app_version)?);
        default_headers.insert(
            "x-pm-drive-sdk-version",
            HeaderValue::from_str(ProtonApiDefaults::sdk_version().as_str())?,
        );
        default_headers.insert(
            "Language",
            HeaderValue::from_str(config.bindings_language.as_deref().unwrap_or("en"))?,
        );
        default_headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.protonmail.v1+json"),
        );

        if let Some((session_id, access_token)) = auth {
            let bearer = format!("Bearer {access_token}");
            default_headers.insert(AUTHORIZATION, HeaderValue::from_str(&bearer)?);
            default_headers.insert("x-pm-uid", HeaderValue::from_str(session_id.raw())?);
        }

        builder = builder.default_headers(default_headers);

        if !config.user_agent.is_empty() {
            builder = builder.user_agent(config.user_agent.clone());
        } else {
            builder = builder.user_agent(ProtonApiDefaults::user_agent());
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

    /// Restores a [`ProtonAPISession`] from previously serialized credentials.
    ///
    /// This is the complement of [`Self::to_stored_credentials`].  Use it on startup to resume a
    /// session that was persisted across process restarts, avoiding a full re-authentication.
    ///
    /// # Arguments
    ///
    /// * `cred` – Credentials returned by a prior call to [`Self::to_stored_credentials`].
    /// * `app_version` – The current application version configuration, used to build the `x-pm-appversion`
    ///   header on every request.
    /// * `options` – Proton client options
    pub fn from_stored_credentials(
        cred: StoredCredentials,
        app_version: AppVersionConfiguration,
        mut options: ProtonClientOptions,
    ) -> Self {
        let secret_cache = options
            .secret_cache_repository
            .take()
            .unwrap_or_else(|| Arc::new(InMemoryCacheRepository::new()));

        ProtonAPISession::resume_with_options(
            SessionId::new(cred.session_id().to_string()),
            cred.username(),
            UserId::new(cred.user_id().to_string()),
            cred.access_token().to_string(),
            cred.refresh_token().to_string(),
            cred.scopes().to_vec(),
            cred.is_waiting_for_second_factor_code(),
            cred.password_mode(),
            app_version,
            secret_cache,
            options,
        )
    }

    /// Serializes the current session state into a [`StoredCredentials`] value that can be
    /// persisted and later handed to [`Self::from_stored_credentials`].
    ///
    /// The snapshot captures the tokens that were active at the time of this call.  If the
    /// session's access token has been silently refreshed since the last explicit store, prefer
    /// calling the async [`TokenCredential::get_tokens`] directly and constructing
    /// [`StoredCredentials`] from those values.
    pub fn to_stored_credentials(&self) -> StoredCredentials {
        StoredCredentials::new(
            self.session_id.raw().clone(),
            self.username.clone(),
            self.user_id.raw().clone(),
            self.token_credential.current_access_token().to_string(),
            self.token_credential.current_refresh_token().to_string(),
            self.scopes.clone(),
            self.is_waiting_for_second_factor_code,
            self.password_mode,
        )
    }

    /// Serializes the current session state using the latest token pair known to the credential.
    pub async fn to_stored_credentials_with_latest_tokens(
        &self,
    ) -> anyhow::Result<StoredCredentials> {
        let (access_token, refresh_token) = self.token_credential.get_tokens().await?;
        Ok(StoredCredentials::new(
            self.session_id.raw().clone(),
            self.username.clone(),
            self.user_id.raw().clone(),
            access_token,
            refresh_token,
            self.scopes.clone(),
            self.is_waiting_for_second_factor_code,
            self.password_mode,
        ))
    }

    /// Sign in through the Proton account website (session fork).
    ///
    /// This is the flow used by the official Drive CLI: the user completes login
    /// (and any CAPTCHA) in a browser, then this method polls until the session
    /// is ready. `on_sign_in` receives the URL to open and the user code to show.
    pub async fn begin_via_web(
        app_version: AppVersionConfiguration,
        options: ProtonClientOptions,
        mut on_sign_in: impl FnMut(&str, &str),
    ) -> anyhow::Result<ProtonAPISession> {
        const AUTH_CLIENT_ID: &str = "external-drive";
        const ACCOUNT_URL: &str = "https://account.proton.me";
        const FORK_INITIAL_DELAY: Duration = Duration::from_secs(5);
        const FORK_POLL_INTERVAL: Duration = Duration::from_secs(5);
        const FORK_MAX_WAIT: Duration = Duration::from_secs(10 * 60);

        let configuration = ProtonClientConfiguration::new(app_version, options)?;
        let auth_api_client = Self::create_authentication_api_client(&configuration)?;
        let fork = auth_api_client.init_session_fork().await?;

        let mut encryption_key = Zeroizing::new([0u8; 32]);
        rand::rng().fill_bytes(encryption_key.as_mut());
        let mut sign_in_url = generate_sign_in_url(AUTH_CLIENT_ID, &fork.user_code, &encryption_key, ACCOUNT_URL);
        on_sign_in(&sign_in_url, &fork.user_code);
        sign_in_url.zeroize();

        tokio::time::sleep(FORK_INITIAL_DELAY).await;
        let started = std::time::Instant::now();
        let status = loop {
            if started.elapsed() > FORK_MAX_WAIT {
                anyhow::bail!("browser sign-in timed out");
            }
            match auth_api_client.poll_session_fork(&fork.selector).await? {
                Some(status) => break status,
                None => tokio::time::sleep(FORK_POLL_INTERVAL).await,
            }
        };

        let key_password = Zeroizing::new(decrypt_fork_key_password(&encryption_key, &status.payload)?);
        encryption_key.zeroize();
        let password_mode = status.password_mode.unwrap_or(PasswordMode::Single);
        let token_credential = TokenCredential::new(
            auth_api_client,
            status.session_id.clone(),
            status.access_token,
            status.refresh_token,
        );
        let mut session = ProtonAPISession::new(
            status.session_id,
            status.user_id.raw().clone(),
            status.user_id.clone(),
            token_credential,
            status.scopes,
            false,
            password_mode,
            configuration,
        );

        // Fork sessions are not "locked", so /keys/salts returns 403. The mailbox
        // password is already in the fork payload; attach it to each user key.
        let user = DefaultUsersApiClient::new_with_token_credential(
            session.http_client.clone(),
            session.token_credential.clone(),
        )
        .get_user()
        .await?
        .user
        .ok_or_else(|| anyhow::anyhow!("missing user after browser sign-in"))?;
        if !user.name.is_empty() {
            session.username = user.name;
        }
        if !user.id.is_empty() {
            session.user_id = UserId::new(user.id);
        }
        for key in &user.keys {
            session
                .session_secret_cache
                .set_account_key_passphrase(&key.id, key_password.as_bytes())
                .await?;
        }
        Ok(session)
    }
    /// Prefer [`Self::from_stored_credentials`] when reviving a persisted session.
    pub fn resume(
        session_id: SessionId,
        username: impl Into<String>,
        user_id: UserId,
        access_token: String,
        refresh_token: String,
        scopes: Vec<String>,
        is_waiting_for_second_factor_code: bool,
        password_mode: PasswordMode,
        app_version: AppVersionConfiguration,
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

    /// Restores a session from stored token material with custom client options.
    /// The `secret_cache_repository` is attached to `options` and used to re-populate the key cache on demand.
    pub fn resume_with_options(
        session_id: SessionId,
        username: impl Into<String>,
        user_id: UserId,
        access_token: String,
        refresh_token: String,
        scopes: Vec<String>,
        is_waiting_for_second_factor_code: bool,
        password_mode: PasswordMode,
        app_version: AppVersionConfiguration,
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

    /// Re-creates a session around new token material returned by the server after a token rotation.
    /// Preserves the username, user ID and client configuration from the expired session.
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

    /// Ends the session from the provided session and options by sending a `DELETE` request
    /// to `auth/v4`
    pub async fn end_from_token(
        id: String,
        access_token: String,
        app_version: AppVersionConfiguration,
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

    /// Applies the second factor code by sending a request to `auth/v4/2fa`.
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

    /// Apply the data password.
    ///
    /// This function unlocks the key salts required.
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

    /// Refreshes scopes, typically used during reauthentication.
    pub async fn refresh_scopes(&mut self) -> anyhow::Result<()> {
        let auth_api_client = Self::create_authentication_api_client(&self.client_config)?;
        let scopes_response = auth_api_client.get_scopes().await?;
        self.scopes = scopes_response.scopes;
        Ok(())
    }

    /// Ensures authentication by sending a request to `/auth/v4/scopes` and
    /// verifying the response to be 200, or to refresh tokens and restart `http_client`.
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
            anyhow::bail!(
                "Session expired: access token could not be refreshed. Please re-authenticate."
            );
        }

        self.http_client = Self::create_http_client(
            &self.client_config,
            Some((&self.session_id, refreshed_access_token.as_str())),
        )?;
        self.keys_api = None;
        self.authentication_api = None;

        Ok(())
    }

    /// Terminates the session by revoking the current access token server-side.
    /// Returns `true` if the session was successfully ended or was already ended.
    pub async fn end_from_session(&mut self) -> anyhow::Result<bool> {
        if self.is_ended {
            return Ok(true);
        }

        let auth_api_client = Self::create_authentication_api_client(&self.client_config)?;
        let _ = auth_api_client.end_session().await?;
        self.is_ended = true;
        Ok(true)
    }

    /// Creates a new http client or takes the http client from self.
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

    /// Utility function for deriving secrets from a password+salt.
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
    /// Returns a lazily-initialised client for the Proton Keys API (`/keys`).
    /// The client is cached on the session after the first call.
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

    /// Returns a lazily-initialised client for the Proton Authentication API (`/auth`).
    /// The client is cached on the session after the first call.
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

fn generate_sign_in_url(
    auth_client_id: &str,
    user_code: &str,
    encryption_key: &[u8; 32],
    account_url: &str,
) -> String {
    let payload = format!(
        "0:{}:{}:{auth_client_id}",
        user_code,
        STANDARD.encode(encryption_key)
    );
    format!("{account_url}/desktop/login?app=drive&pv=3#payload={}", encode_uri_component(&payload))
}

fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn decrypt_fork_key_password(encryption_key: &[u8; 32], encoded_payload: &str) -> anyhow::Result<String> {
    let blob = STANDARD.decode(encoded_payload.as_bytes())?;
    const NONCE_LEN: usize = 12;
    const TAG_LEN: usize = 16;
    if blob.len() < NONCE_LEN + TAG_LEN {
        anyhow::bail!("invalid fork payload blob length");
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new_from_slice(encryption_key)?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad: b"fork",
            },
        )
        .map_err(|_| anyhow::anyhow!("failed to decrypt fork payload"))?;
    let parsed: serde_json::Value = serde_json::from_slice(&plaintext)?;
    parsed
        .get("keyPassword")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("fork payload missing keyPassword"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::Aead;

    #[test]
    fn fork_payload_roundtrip() {
        let key = [7u8; 32];
        let nonce_bytes = [3u8; 12];
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let nonce = Nonce::from_slice(&nonce_bytes);
        let body = br#"{"keyPassword":"mailbox-secret"}"#;
        let ct = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: body,
                    aad: b"fork",
                },
            )
            .unwrap();
        let mut blob = nonce_bytes.to_vec();
        blob.extend_from_slice(&ct);
        let encoded = STANDARD.encode(blob);
        let password = decrypt_fork_key_password(&key, &encoded).unwrap();
        assert_eq!(password, "mailbox-secret");
    }
}
