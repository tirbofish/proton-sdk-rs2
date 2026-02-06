use std::{sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{PasswordMode, SessionId, UserId, auth::TokenCredential, cache::CacheRepositoryTrait, client::{ProtonClientConfiguration, ProtonClientOptions}, secret::SessionSecretCache};

pub struct ProtonAPISession {
    session_id: SessionId,
    username: String,
    user_id: UserId,
    token_credential: TokenCredential,
    scopes: Vec<String>,
    is_waiting_for_second_factor_code: bool,
    password_mode: PasswordMode,
    client_config: ProtonClientConfiguration,
    secret_cache: SessionSecretCache,
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
        let secret_cache = SessionSecretCache::new(client_config.secret_cache_repository.clone());
        
        Self {
            session_id,
            username,
            user_id,
            token_credential,
            scopes,
            is_waiting_for_second_factor_code,
            password_mode,
            client_config,
            secret_cache,
        }
    }
    
    pub async fn begin(
        username: impl Into<String>,
        password: &[u8],
        app_version: semver::Version,
        session_options: ProtonSessionOptions,
    ) -> anyhow::Result<ProtonAPISession> {
        todo!()
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
        secret_cache_repository: Arc<dyn CacheRepositoryTrait>,
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
        secret_cache_repository: Arc<dyn CacheRepositoryTrait>,
        options: ProtonClientOptions,
    ) -> ProtonAPISession {
        todo!()
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
        todo!()
    }

    pub async fn end_from_token(
        id: String,
        access_token: String,
        app_version: semver::Version,
        options: Option<ProtonClientOptions>,
    ) -> anyhow::Result<()> {
        todo!()
    }

    pub async fn apply_second_factor_code(
        &mut self,
        second_factor_code: String, 
        cancellation_token: CancellationToken
    ) -> anyhow::Result<()> {
        todo!()
    }

    pub async fn apply_data_password(
        &mut self,
        password: &[u8],
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<()> {
        todo!()
    }

    pub async fn refresh_scopes(cancellation_token: CancellationToken) -> anyhow::Result<()> {
        todo!()
    }

    pub async fn end_from_session(
        &self
    ) -> anyhow::Result<bool> {
        todo!()
    }

    pub(crate) fn get_http_client(base_route_path: Option<String>, attempt_timeout: Option<Duration>, total_timeout: Option<Duration>) -> HttpClient {
        todo!()
    }

    pub(crate) fn derive_secret_from_password(password: &[u8], salt: &[u8]) -> Vec<u8> {
        todo!()
    }

    fn on_refresh_token_expired(&mut self) {
        todo!()
    }
}

pub struct ProtonSessionOptions {
    pub client: ProtonClientOptions,
    pub secret_cache_repository: Option<Arc<dyn CacheRepositoryTrait>>,
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