use std::{sync::Arc, time::Duration};

use http::Uri;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::{
    addresses::DefaultAddressesApiClient,
    api::ApiClientFactory,
    auth::DefaultAuthenticationApiClient,
    cache::{CacheRepository, InMemoryCacheRepository},
    keys::DefaultKeysApiClient,
    protobuf::ProtonClientTlsPolicy,
    session::ProtonAPISession,
    users::DefaultUsersApiClient,
};

pub struct ProtonClientOptions {
    pub base_url: Option<http::Uri>,
    pub user_agent: Option<String>,
    pub tls_policy: Option<ProtonClientTlsPolicy>,
    pub custom_http_message_handler_factory:
        Option<Arc<dyn Fn() -> Box<dyn HttpMessageHandler> + Send + Sync>>,
    pub entity_cache_repository: Option<Arc<dyn CacheRepository>>,
    pub telemetry: Option<Arc<dyn Telemetry>>,
    pub feature_flag_provider: Option<Arc<dyn FeatureFlagProvider>>,

    pub secret_cache_repository: Option<Arc<dyn CacheRepository>>,
    pub refresh_redirect_uri: Option<http::Uri>,
    pub bindings_language: Option<String>,
}

pub struct ProtonClientConfiguration {
    pub base_url: http::Uri,
    pub app_version: semver::Version,
    pub user_agent: String,
    pub tls_policy: ProtonClientTlsPolicy,
    pub custom_http_message_handler_factory:
        Option<Arc<dyn Fn() -> Box<dyn HttpMessageHandler> + Send + Sync>>,
    pub secret_cache_repository: Arc<dyn CacheRepository>,
    pub entity_cache_repository: Arc<dyn CacheRepository>,
    pub telemetry: Arc<dyn Telemetry>,
    pub feature_flag_provider: Arc<dyn FeatureFlagProvider>,
    pub refresh_redirect_uri: http::Uri,
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
    pub fn new(app_version: semver::Version, options: ProtonClientOptions) -> anyhow::Result<Self> {
        Ok(Self {
            base_url: options.base_url.unwrap_or(ProtonApiDefaults::base_url()),
            app_version,
            user_agent: options.user_agent.unwrap_or(String::new()),
            tls_policy: options.tls_policy.unwrap_or(ProtonClientTlsPolicy::Strict),
            custom_http_message_handler_factory: options.custom_http_message_handler_factory, // todo: dont make this null, make it smth else
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
        let app_version_header = Self::build_pm_app_version(self);
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

        #[cfg(feature = "danger-disable-tls-validation")]
        if !matches!(self.tls_policy, ProtonClientTlsPolicy::Strict) {
            log::warn!(
                "TLS certificate validation is disabled (policy: {:?}). \
                 This is unsafe for production use.",
                self.tls_policy
            );
            builder = builder.danger_accept_invalid_certs(true);
        }

        // TODO: Custom message handlers, retry/circuit-breaker policies, cookie container,
        // and session authorization middleware require additional middleware abstractions.
        builder.build().map_err(Into::into)
    }

    pub(crate) fn build_pm_app_version(config: &ProtonClientConfiguration) -> String {
        let binding_segment = config
            .bindings_language
            .as_deref()
            .unwrap_or("rust")
            .chars()
            .map(|c| {
                if c.is_ascii_alphabetic() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
            .trim_matches('_')
            .to_string();

        let binding_segment = if binding_segment.is_empty() {
            "rust".to_string()
        } else {
            binding_segment
        };

        let platform = format!("external-drive-{}", binding_segment);
        let pre = config.app_version.pre.as_str();
        let pre_lower = pre.to_ascii_lowercase();

        let (channel, suffix) = if pre_lower.starts_with("alpha") {
            (Some("alpha"), &pre[5..])
        } else if pre_lower.starts_with("beta") {
            (Some("beta"), &pre[4..])
        } else if pre_lower.starts_with("rc") {
            (Some("RC"), &pre[2..])
        } else if pre_lower.starts_with("stable") {
            (Some("stable"), &pre[6..])
        } else if pre.is_empty() {
            (None, "")
        } else {
            (Some("alpha"), pre)
        };

        let numeric_suffix: String = suffix
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        let dev_suffix = if pre_lower.contains("dev") {
            ".dev"
        } else {
            ""
        };

        let mut value = format!(
            "{}@{}.{}.{}",
            platform, config.app_version.major, config.app_version.minor, config.app_version.patch,
        );

        if let Some(ch) = channel {
            value.push('-');
            value.push_str(ch);
            value.push_str(&numeric_suffix);
        }

        value.push_str(dev_suffix);

        if !config.app_version.build.is_empty() {
            value.push('+');
            value.push_str(config.app_version.build.as_str());
        }

        value
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
