use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::{
    cache::{CacheRepositoryTrait, InMemoryCacheRepository}, proton::{self, ProtonClientTlsPolicy}
};

// the protos are bad
pub struct ProtonClientOptions {
    pub base_url: Option<http::Uri>,
    pub user_agent: Option<String>,
    pub tls_policy: Option<ProtonClientTlsPolicy>,
    pub custom_http_message_handler_factory:
        Option<Arc<dyn Fn() -> Box<dyn HttpMessageHandler> + Send + Sync>>,
    pub entity_cache_repository: Option<Arc<dyn CacheRepositoryTrait>>,
    pub telemetry: Option<Arc<dyn TelemetryTrait>>,
    pub feature_flag_provider: Option<Arc<dyn FeatureFlagProvider>>,

    pub(crate) secret_cache_repository: Option<Arc<dyn CacheRepositoryTrait>>,
    pub(crate) refresh_redirect_uri: Option<http::Uri>,
    pub(crate) bindings_language: Option<String>,
}

pub struct ProtonClientConfiguration {
    pub base_url: http::Uri,
    pub app_version: semver::Version,
    pub user_agent: String,
    pub tls_policy: ProtonClientTlsPolicy,
    pub custom_http_message_handler_factory:
        Option<Arc<dyn Fn() -> Box<dyn HttpMessageHandler> + Send + Sync>>,
    pub secret_cache_repository: Arc<dyn CacheRepositoryTrait>,
    pub entity_cache_repository: Arc<dyn CacheRepositoryTrait>,
    pub telemetry: Arc<dyn TelemetryTrait>,
    pub feature_flag_provider: Arc<dyn FeatureFlagProvider>,
    pub refresh_redirect_uri: http::Uri,
    pub bindings_language: Option<String>,
}

impl Default for ProtonClientOptions {
    fn default() -> Self {
        Self { base_url: Default::default(), user_agent: Default::default(), tls_policy: Default::default(), custom_http_message_handler_factory: Default::default(), entity_cache_repository: Default::default(), telemetry: Default::default(), feature_flag_provider: Default::default(), secret_cache_repository: Default::default(), refresh_redirect_uri: Default::default(), bindings_language: Default::default() }
    }
}

impl ProtonClientConfiguration {
    pub fn new(
        app_version: semver::Version,
        options: ProtonClientOptions,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            base_url: options.base_url.unwrap_or(ProtonApiDefaults::base_url()),
            app_version,
            user_agent: options.user_agent.unwrap_or(String::new()),
            tls_policy: options.tls_policy.unwrap_or(ProtonClientTlsPolicy::Strict),
            custom_http_message_handler_factory: options.custom_http_message_handler_factory, // todo: dont make this null, make it smth else
            secret_cache_repository: options.secret_cache_repository.unwrap_or(Arc::new(InMemoryCacheRepository::new())),
            entity_cache_repository: options.entity_cache_repository.unwrap_or(Arc::new(InMemoryCacheRepository::new())),
            telemetry: options.telemetry.unwrap_or(Arc::new(NullTelemetry {})),
            feature_flag_provider: Arc::new(AlwaysDisabledFeatureFlagProvider),
            refresh_redirect_uri: options.refresh_redirect_uri.unwrap_or(ProtonApiDefaults::refresh_redirect_uri()),
            bindings_language: options.bindings_language.clone(),
        })
    }
}

pub struct ProtonApiDefaults;

impl ProtonApiDefaults {
    pub const DEFAULT_TIMEOUT_SECONDS: u32 = 30;

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
pub trait TelemetryTrait: Send + Sync {
    async fn record_metric(&self, name: String, payload: Option<Vec<u8>>);
}

pub struct NullTelemetry;

#[async_trait::async_trait]
impl TelemetryTrait for NullTelemetry {
    async fn record_metric(&self, _name: String, _payload: Option<Vec<u8>>) {}
}

#[async_trait::async_trait]
pub trait FeatureFlagProvider: Send + Sync {
    async fn is_enabled(
        &self,
        flag_name: String,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<bool>;
}

pub struct AlwaysDisabledFeatureFlagProvider;

#[async_trait::async_trait]
impl FeatureFlagProvider for AlwaysDisabledFeatureFlagProvider {
    async fn is_enabled(
        &self,
        _flag_name: String,
        _cancellation_token: CancellationToken,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }
}