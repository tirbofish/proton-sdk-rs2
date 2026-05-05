use std::{sync::Arc, time::Duration};

use http::Uri;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::{
    addresses::DefaultAddressesApiClient,
    api::ApiClientFactory,
    auth::DefaultAuthenticationApiClient,
    cache::{CacheRepository, InMemoryCacheRepository},
    keys::DefaultKeysApiClient,
    session::ProtonAPISession,
    users::DefaultUsersApiClient,
    utils::AppVersionConfiguration,
};

pub struct ProtonClientOptions {
    /// Overrides the default Proton API base URL (`https://mail.proton.me`).
    pub base_url: Option<http::Uri>,
    /// Custom `User-Agent` header value sent with every request.
    pub user_agent: Option<String>,
    /// TLS verification strictness; defaults to `Strict`.
    pub tls_policy: Option<ProtonClientTlsPolicy>,
    /// Optional custom HTTP handler factory (e.g. for certificate pinning or request logging).
    pub custom_http_message_handler_factory:
        Option<Arc<dyn Fn() -> Box<dyn HttpMessageHandler> + Send + Sync>>,
    /// Repository used to persist entity data such as shares and nodes between sessions.
    pub entity_cache_repository: Option<Arc<dyn CacheRepository>>,
    /// Sink for telemetry events emitted by the SDK.
    pub telemetry: Option<Arc<dyn Telemetry>>,
    /// Provider queried before gating experimental features.
    pub feature_flag_provider: Option<Arc<dyn FeatureFlagProvider>>,
    /// Repository used to persist sensitive key material (passphrases, session keys).
    pub secret_cache_repository: Option<Arc<dyn CacheRepository>>,
    /// URI that the token-refresh flow redirects to after a successful refresh.
    pub refresh_redirect_uri: Option<http::Uri>,
    /// BCP-47 language tag forwarded by language bindings (e.g. `"en-US"`).
    pub bindings_language: Option<String>,
}

#[derive(Clone)]
pub struct ProtonClientConfiguration {
    /// Base URL used for the drive API.
    pub base_url: http::Uri,
    /// Application version configuration used to build the `x-pm-appversion` header.
    pub app_version: AppVersionConfiguration,
    /// `User-Agent` string sent with every request.
    pub user_agent: String,
    /// TLS verification policy in use; strict by default.
    pub tls_policy: ProtonClientTlsPolicy,
    /// Optional custom HTTP message handler factory (pinning, logging, etc.).
    pub custom_http_message_handler_factory:
        Option<Arc<dyn Fn() -> Box<dyn HttpMessageHandler> + Send + Sync>>,
    /// Repository used to store decrypted key material across restarts.
    pub secret_cache_repository: Arc<dyn CacheRepository>,
    /// Repository used to store entity metadata (shares, nodes, volumes) across restarts.
    pub entity_cache_repository: Arc<dyn CacheRepository>,
    /// Telemetry sink; a no-op implementation is used when none is provided.
    pub telemetry: Arc<dyn Telemetry>,
    /// Feature-flag provider; all flags are disabled by default.
    pub feature_flag_provider: Arc<dyn FeatureFlagProvider>,
    /// URI the server redirects to after a successful token refresh.
    pub refresh_redirect_uri: http::Uri,
    /// Optional BCP-47 language tag forwarded by language bindings.
    pub bindings_language: Option<String>,
}

impl Default for ProtonClientOptions {
    fn default() -> Self {
        Self {
            base_url: Default::default(),
            user_agent: Default::default(),
            tls_policy: Default::default(),
            custom_http_message_handler_factory: Default::default(),
            entity_cache_repository: Default::default(),
            telemetry: Default::default(),
            feature_flag_provider: Default::default(),
            secret_cache_repository: Default::default(),
            refresh_redirect_uri: Default::default(),
            bindings_language: Default::default(),
        }
    }
}

impl ProtonClientConfiguration {
    pub fn new(
        app_version: AppVersionConfiguration,
        options: ProtonClientOptions,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            base_url: options.base_url.unwrap_or(ProtonApiDefaults::base_url()),
            app_version,
            user_agent: options.user_agent.unwrap_or(String::new()),
            tls_policy: options.tls_policy.unwrap_or(ProtonClientTlsPolicy::Strict),
            custom_http_message_handler_factory: options.custom_http_message_handler_factory,
            secret_cache_repository: options
                .secret_cache_repository
                .unwrap_or(Arc::new(InMemoryCacheRepository::new())),
            entity_cache_repository: options
                .entity_cache_repository
                .unwrap_or(Arc::new(InMemoryCacheRepository::new())),
            telemetry: options.telemetry.unwrap_or(Arc::new(NullTelemetry {})),
            feature_flag_provider: Arc::new(AlwaysDisabledFeatureFlagProvider),
            refresh_redirect_uri: options
                .refresh_redirect_uri
                .unwrap_or(ProtonApiDefaults::refresh_redirect_uri()),
            bindings_language: options.bindings_language.clone(),
        })
    }

    pub fn get_http_client(
        &self,
        _session: Option<&ProtonAPISession>,
        base_route_path: Option<String>,
        attempt_timeout: Option<Duration>,
        total_timeout: Option<Duration>,
    ) -> anyhow::Result<reqwest::Client> {
        let base_route_path = base_route_path.unwrap_or_default();
        let _base_address =
            reqwest::Url::parse(&self.base_url.to_string())?.join(base_route_path.as_str())?;

        let mut default_headers = HeaderMap::new();
        let app_version_header = self.app_version.to_string();
        log::debug!("Generated x-pm-appversion header: {}", app_version_header);
        default_headers.insert(
            "x-pm-appversion",
            HeaderValue::from_str(app_version_header.as_str())?,
        );

        let mut builder = reqwest::Client::builder().default_headers(default_headers);

        if !self.user_agent.is_empty() {
            builder = builder.user_agent(self.user_agent.clone());
        } else {
            builder = builder.user_agent(ProtonApiDefaults::user_agent());
        }

        let _base_address =
            reqwest::Url::parse(&self.base_url.to_string())?.join(base_route_path.as_str())?;

        if let Some(connect_timeout) = attempt_timeout {
            builder = builder.connect_timeout(connect_timeout);
        }

        if let Some(total_request_timeout) = total_timeout {
            builder = builder.timeout(total_request_timeout);
        }

        builder = builder
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(std::time::Duration::from_secs(90));

        if !matches!(self.tls_policy, ProtonClientTlsPolicy::Strict) {
            builder = builder.danger_accept_invalid_certs(true);
        }

        builder.build().map_err(Into::into)
    }
}

pub struct ProtonApiDefaults;

impl ProtonApiDefaults {
    pub const DEFAULT_TIMEOUT_SECONDS: u32 = 30;

    pub fn user_agent() -> String {
        format!(
            "ProtonDriveSDK/{} (Rust; {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS
        )
    }

    pub fn base_url() -> http::Uri {
        "https://drive-api.proton.me/"
            .parse()
            .expect("Invalid default base URL")
    }

    pub fn refresh_redirect_uri() -> http::Uri {
        "https://proton.me"
            .parse()
            .expect("Invalid default refresh redirect URI")
    }
}

pub trait HttpMessageHandler: Send + Sync {}

#[async_trait::async_trait]
pub trait Telemetry: Send + Sync {
    async fn record_metric(&self, name: String, payload: Option<Vec<u8>>);
}

pub struct NullTelemetry;

#[async_trait::async_trait]
impl Telemetry for NullTelemetry {
    async fn record_metric(&self, _name: String, _payload: Option<Vec<u8>>) {}
}

#[async_trait::async_trait]
pub trait FeatureFlagProvider: Send + Sync {
    async fn is_enabled(&self, flag_name: String) -> anyhow::Result<bool>;
}

pub struct AlwaysDisabledFeatureFlagProvider;

#[async_trait::async_trait]
impl FeatureFlagProvider for AlwaysDisabledFeatureFlagProvider {
    async fn is_enabled(&self, _flag_name: String) -> anyhow::Result<bool> {
        Ok(false)
    }
}

pub struct ApiClient;

impl ApiClient {
    pub fn create_authentication_api_client(
        http_client: reqwest::Client,
        refresh_redirect_uri: Uri,
    ) -> DefaultAuthenticationApiClient {
        DefaultAuthenticationApiClient::new(http_client, refresh_redirect_uri)
    }

    pub fn create_keys_api_client(http_client: reqwest::Client) -> DefaultKeysApiClient {
        DefaultKeysApiClient::new(http_client)
    }

    pub fn create_users_api_client(http_client: reqwest::Client) -> DefaultUsersApiClient {
        DefaultUsersApiClient::new(http_client)
    }

    pub fn create_addresses_api_client(http_client: reqwest::Client) -> DefaultAddressesApiClient {
        DefaultAddressesApiClient::new(http_client)
    }
}

impl ApiClientFactory for ApiClient {}

#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
#[repr(i32)]
pub enum ProtonClientTlsPolicy {
    Strict = 0,
    NoCertificatePinning = 1,
    NoCertificateValidation = 2,
}
