use std::fmt::{Display, Formatter};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Release channel for the application version.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum ReleaseChannel {
    /// Stable production release.
    #[default]
    Stable,
    /// Beta/preview release.
    Beta,
    /// Release candidate.
    RC,
    /// Alpha/early development release.
    Alpha,
}

impl Display for ReleaseChannel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ReleaseChannel::Stable => write!(f, "stable"),
            ReleaseChannel::Beta => write!(f, "beta"),
            ReleaseChannel::RC => write!(f, "RC"),
            ReleaseChannel::Alpha => write!(f, "alpha"),
        }
    }
}

/// Allows for an easy construction of the `x-pm-appversion` header that conforms to the guidelines:
///
/// > Set the x-pm-appversion HTTP header using the format `external-drive-{projectname}@{version}`
/// > (e.g., `external-drive-myapp@1.2.3`). This header must accurately represent your
/// > application. Do not spoof or falsify this value.
///
/// The version string must match this regex:
/// ```text
/// ^(external-drive)+(-[a-z_]+)+@[0-9]+\.[0-9]+\.[0-9]+(\.[0-9]+)?-((stable|beta|RC|alpha)(([.-]?\d+)*)?)?([.-]?dev)?(\+.*)?$
/// ```
///
/// # Examples
/// - `external-drive-myapp@1.2.3-stable`
/// - `external-drive-swift_sdk@1.2.3.4-beta1`
/// - `external-drive-rust@1.0.0-beta2.dev`
/// - `external-drive-android_sdk@2.0.0-RC1+build.456`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppVersionConfiguration {
    /// Project name segments (e.g., "myapp", "swift_sdk").
    /// Will be normalized to lowercase with non-alphanumeric characters replaced by underscores.
    /// 
    /// The `android_sdk` in `external-drive-android_sdk@2.0.0-RC1+build.456`
    pub app_name: String,
    /// Major version number.
    /// 
    /// The `2` in `external-drive-android_sdk@2.0.0-RC1+build.456`
    pub major: u64,
    /// Minor version number.
    /// 
    /// The first `0` in `external-drive-android_sdk@2.0.0-RC1+build.456`
    pub minor: u64,
    /// Patch version number.
    /// 
    /// The second `0` in `external-drive-android_sdk@2.0.0-RC1+build.456`
    pub patch: u64,
    /// Optional build number (fourth version component).
    /// 
    /// The `789` in `external-drive-myapp@1.2.3.789-beta2`
    pub build_number: Option<u64>,
    /// Release channel (stable, beta, RC, alpha).
    /// 
    /// The `RC` in `external-drive-android_sdk@2.0.0-RC1+build.456`
    pub channel: Option<ReleaseChannel>,
    /// Optional numeric suffix for the channel (e.g., "1" in "beta1", "2.3" in "RC2.3").
    /// 
    /// The `1` in `external-drive-android_sdk@2.0.0-RC1+build.456`
    pub channel_suffix: Option<String>,
    /// Whether this is a development build.
    /// 
    /// The `dev` in `external-drive-myapp@1.0.0-dev`
    pub is_dev: bool,
    /// Optional build metadata (appears after `+`).
    /// 
    /// The `build.456` in `external-drive-android_sdk@2.0.0-RC1+build.456`
    pub build_metadata: Option<String>,
}

impl Default for AppVersionConfiguration {
    fn default() -> Self {
        Self {
            app_name: String::from("rust"),
            major: 0,
            minor: 1,
            patch: 0,
            build_number: None,
            channel: Some(ReleaseChannel::Alpha),
            channel_suffix: None,
            is_dev: true,
            build_metadata: None,
        }
    }
}

impl AppVersionConfiguration {
    /// Creates a new `AppVersionConfiguration` with the required fields.
    pub fn new(app_name: impl Into<String>, major: u64, minor: u64, patch: u64) -> Self {
        Self {
            app_name: app_name.into(),
            major,
            minor,
            patch,
            build_number: None,
            channel: None,
            channel_suffix: None,
            is_dev: false,
            build_metadata: None,
        }
    }

    /// Sets the release channel.
    pub fn with_channel(mut self, channel: ReleaseChannel) -> Self {
        self.channel = Some(channel);
        self
    }

    /// Sets the channel suffix (e.g., "1" for "beta1").
    pub fn with_channel_suffix(mut self, suffix: impl Into<String>) -> Self {
        self.channel_suffix = Some(suffix.into());
        self
    }

    /// Sets the build number (fourth version component).
    pub fn with_build_number(mut self, build_number: u64) -> Self {
        self.build_number = Some(build_number);
        self
    }

    /// Marks this as a development build.
    pub fn with_dev(mut self) -> Self {
        self.is_dev = true;
        self
    }

    /// Sets build metadata (appears after `+`).
    pub fn with_build_metadata(mut self, metadata: impl Into<String>) -> Self {
        self.build_metadata = Some(metadata.into());
        self
    }

    /// Creates a configuration from a `semver::Version` and app name.
    /// Parses the prerelease segment to extract channel information.
    pub fn from_semver(app_name: impl Into<String>, version: &semver::Version) -> Self {
        let pre = version.pre.as_str();
        let pre_lower = pre.to_ascii_lowercase();

        let (channel, suffix) = if pre_lower.starts_with("alpha") {
            (Some(ReleaseChannel::Alpha), &pre[5..])
        } else if pre_lower.starts_with("beta") {
            (Some(ReleaseChannel::Beta), &pre[4..])
        } else if pre_lower.starts_with("rc") {
            (Some(ReleaseChannel::RC), &pre[2..])
        } else if pre_lower.starts_with("stable") {
            (Some(ReleaseChannel::Stable), &pre[6..])
        } else if pre.is_empty() {
            (None, "")
        } else {
            // Unknown prerelease, treat as alpha
            (Some(ReleaseChannel::Alpha), pre)
        };

        let channel_suffix: String = suffix
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        let channel_suffix = if channel_suffix.is_empty() {
            None
        } else {
            Some(channel_suffix)
        };

        let is_dev = pre_lower.contains("dev");

        let build_metadata = if version.build.is_empty() {
            None
        } else {
            Some(version.build.as_str().to_string())
        };

        Self {
            app_name: app_name.into(),
            major: version.major,
            minor: version.minor,
            patch: version.patch,
            build_number: None,
            channel,
            channel_suffix,
            is_dev,
            build_metadata,
        }
    }

    /// Normalizes the app name: lowercase, replace non-alphanumeric with underscores.
    fn normalized_app_name(&self) -> String {
        self.app_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
            .trim_matches('_')
            .to_string()
    }
}

impl Display for AppVersionConfiguration {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let app_name = self.normalized_app_name();
        let app_name = if app_name.is_empty() { "rust" } else { &app_name };

        // Base: external-drive-{appname}@{major}.{minor}.{patch}
        write!(f, "external-drive-{}@{}.{}.{}", app_name, self.major, self.minor, self.patch)?;

        // Optional build number (fourth component)
        if let Some(build_num) = self.build_number {
            write!(f, ".{}", build_num)?;
        }

        // Channel with optional suffix
        if let Some(ref channel) = self.channel {
            write!(f, "-{}", channel)?;
            if let Some(ref suffix) = self.channel_suffix {
                write!(f, "{}", suffix)?;
            }
        }

        // Dev marker
        if self.is_dev {
            write!(f, ".dev")?;
        }

        // Build metadata
        if let Some(ref metadata) = self.build_metadata {
            write!(f, "+{}", metadata)?;
        }

        Ok(())
    }
}



/// If a type has a representation as a protobuf, it will convert to that type.
pub trait AsProtobuf<To> {
    /// Converts `self` to a protobuf.
    fn as_protobuf(&self) -> To;
}

/// Converts from a protobuf type (as denoted with [`From`]) into the rust representation.
pub trait FromProtobuf<From> {
    /// Converts from a protobuf value into `Self`/the rust-native type.
    fn from_protobuf(value: &From) -> Self;
}

pub mod response_result_serde {
    #[allow(deprecated)]
    use crate::protobuf::Error;
    #[allow(deprecated)]
    use crate::protobuf::response::Result as ResponseResult;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "PascalCase")]
    enum ResultHelper {
        Value(super::any_serde_helper::AnyHelper),
        Error(Error),
    }

    pub fn serialize<S: Serializer>(t: &Option<ResponseResult>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(ResponseResult::Value(any)) => {
                s.serialize_some(&ResultHelper::Value(super::any_serde_helper::AnyHelper {
                    type_url: any.type_url.clone(),
                    value: any.value.clone(),
                }))
            }
            Some(ResponseResult::Error(err)) => s.serialize_some(&ResultHelper::Error(err.clone())),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<ResponseResult>, D::Error> {
        let h = Option::<ResultHelper>::deserialize(d)?;
        Ok(h.map(|h| match h {
            ResultHelper::Value(any) => ResponseResult::Value(super::Any {
                type_url: any.type_url,
                value: any.value,
            }),
            ResultHelper::Error(err) => ResponseResult::Error(err),
        }))
    }
}

pub mod any_serde_helper {
    use serde::{Deserialize, Serialize};
    #[derive(Serialize, Deserialize)]
    pub struct AnyHelper {
        pub type_url: String,
        pub value: Vec<u8>,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct Timestamp {
    pub seconds: i64,
    pub nanos: i32,
}

impl prost::Message for Timestamp {
    fn encode_raw<B: prost::bytes::BufMut>(&self, _buf: &mut B) {}
    fn merge_field<B: prost::bytes::Buf>(
        &mut self,
        _tag: u32,
        _wire_type: prost::encoding::WireType,
        _buf: &mut B,
        _ctx: prost::encoding::DecodeContext,
    ) -> Result<(), prost::DecodeError> {
        Ok(())
    }
    fn encoded_len(&self) -> usize {
        0
    }
    fn clear(&mut self) {}
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct Any {
    pub type_url: String,
    pub value: Vec<u8>,
}

impl prost::Message for Any {
    fn encode_raw<B: prost::bytes::BufMut>(&self, _buf: &mut B) {}
    fn merge_field<B: prost::bytes::Buf>(
        &mut self,
        _tag: u32,
        _wire_type: prost::encoding::WireType,
        _buf: &mut B,
        _ctx: prost::encoding::DecodeContext,
    ) -> Result<(), prost::DecodeError> {
        Ok(())
    }
    fn encoded_len(&self) -> usize {
        0
    }
    fn clear(&mut self) {}
}

pub(crate) async fn decode_json<T: DeserializeOwned>(
    response: reqwest::Response,
) -> anyhow::Result<T> {
    let body = response.bytes().await?;
    Ok(serde_json::from_slice::<T>(&body)?)
}

#[macro_export]
macro_rules! define_id {
    ($t:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $t(String);

        impl $t {
            pub fn new(value: String) -> Self {
                Self(value)
            }
            pub fn raw(&self) -> &str {
                &self.0
            }
        }

        impl Default for $t {
            fn default() -> Self {
                Self(String::new())
            }
        }
    };
}
