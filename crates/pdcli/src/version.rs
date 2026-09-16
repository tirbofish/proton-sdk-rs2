use proton_drive_sdk::proton_sdk_rs2::AppVersionConfiguration;

pub const APP_NAME: &str = "pdcli";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Keep the version sent to Proton in lockstep with the binary package.
pub fn app_version_configuration() -> AppVersionConfiguration {
    let mut parts = APP_VERSION.split('.');
    let major = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let minor = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let patch = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    AppVersionConfiguration::new(APP_NAME, major, minor, patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_package_version() {
        assert_eq!(APP_VERSION, env!("CARGO_PKG_VERSION"));
        let config = app_version_configuration();
        assert_eq!(config.app_name, APP_NAME);
        assert_eq!(
            config.major,
            APP_VERSION
                .split('.')
                .next()
                .unwrap()
                .parse::<u64>()
                .unwrap()
        );
    }
}
