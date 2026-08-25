use crate::account::{AddressId, AddressKeyId};
use crate::api::ApiResponse;
use crate::links::LinkId;
use crate::pgp::{PgpArmoredMessage, PgpArmoredPrivateKey, PgpArmoredSignature};
use crate::share::ShareId;
use crate::volume::VolumeId;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use proton_sdk_rs2::auth::TokenCredential;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum DeviceType {
    Windows = 1,
    MacOS = 2,
    Linux = 3,
}

/// Request body for `POST /drive/devices`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreateDeviceRequest {
    pub device: CreateDeviceParams,
    pub share: CreateDeviceShareParams,
    pub link: CreateDeviceLinkParams,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreateDeviceParams {
    pub r#type: DeviceType,
    pub sync_state: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreateDeviceShareParams {
    #[serde(rename = "AddressID")]
    pub address_id: AddressId,

    #[serde(rename = "AddressKeyID")]
    pub address_key_id: AddressKeyId,

    pub key: PgpArmoredPrivateKey,
    pub passphrase: PgpArmoredMessage,
    pub passphrase_signature: PgpArmoredSignature,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreateDeviceLinkParams {
    pub name: PgpArmoredMessage,
    pub node_key: PgpArmoredPrivateKey,
    pub node_passphrase: PgpArmoredMessage,
    pub node_passphrase_signature: PgpArmoredSignature,
    pub node_hash_key: PgpArmoredMessage,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreateDeviceResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    pub device: CreatedDeviceDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreatedDeviceDto {
    #[serde(rename = "DeviceID")]
    pub device_id: String,

    #[serde(rename = "LinkID")]
    pub link_id: LinkId,

    #[serde(rename = "ShareID")]
    pub share_id: ShareId,
}

/// One entry in the `GET /drive/devices` response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceListEntry {
    pub device: DeviceDto,
    pub share: DeviceShareDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceDto {
    #[serde(rename = "DeviceID")]
    pub device_id: String,

    #[serde(rename = "VolumeID")]
    pub volume_id: VolumeId,

    pub r#type: DeviceType,

    #[serde(rename = "CreateTime")]
    pub create_time: i64,

    #[serde(rename = "LastSyncTime")]
    pub last_sync_time: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceShareDto {
    #[serde(rename = "ShareID")]
    pub share_id: ShareId,

    #[serde(rename = "LinkID")]
    pub link_id: LinkId,

    /// Non-empty for "old" devices where the name was stored on the share
    /// rather than on the root node.
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GetDevicesResponse {
    #[serde(flatten)]
    pub base: ApiResponse,

    pub devices: Vec<DeviceListEntry>,
}

#[async_trait]
pub trait DevicesApiClient: Send + Sync {
    async fn get_devices(&self) -> anyhow::Result<GetDevicesResponse>;

    async fn create_device(
        &self,
        request: CreateDeviceRequest,
    ) -> anyhow::Result<CreateDeviceResponse>;

    async fn delete_device(&self, device_id: &str) -> anyhow::Result<()>;

    /// Clears the deprecated share-level name (used during device rename when
    /// the old "Name" field on the share needs to be wiped).
    async fn clear_device_share_name(&self, device_id: &str) -> anyhow::Result<()>;
}

pub struct DefaultDevicesApiClient {
    client: ClientWithMiddleware,
    base_url: reqwest::Url,
    token_credential: Option<TokenCredential>,
}

impl DefaultDevicesApiClient {
    pub fn new(
        client: ClientWithMiddleware,
        base_url: reqwest::Url,
        token_credential: Option<TokenCredential>,
    ) -> Self {
        Self {
            client,
            base_url,
            token_credential,
        }
    }

    async fn add_auth(
        &self,
        mut builder: reqwest_middleware::RequestBuilder,
    ) -> anyhow::Result<reqwest_middleware::RequestBuilder> {
        if let Some(cred) = &self.token_credential {
            let (token, _) = cred.get_tokens().await?;
            builder = builder.header("Authorization", format!("Bearer {token}"));
            builder = builder.header("x-pm-uid", cred.session_id().raw());
        }
        Ok(builder)
    }
}

#[async_trait]
impl DevicesApiClient for DefaultDevicesApiClient {
    async fn get_devices(&self) -> anyhow::Result<GetDevicesResponse> {
        let url = self.base_url.join("devices")?;
        let builder = self.add_auth(self.client.get(url)).await?;
        Ok(builder.send().await?.json().await?)
    }

    async fn create_device(
        &self,
        request: CreateDeviceRequest,
    ) -> anyhow::Result<CreateDeviceResponse> {
        let url = self.base_url.join("devices")?;
        let body = serde_json::to_vec(&request)?;
        let builder = self
            .add_auth(
                self.client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .body(body),
            )
            .await?;
        Ok(builder.send().await?.json().await?)
    }

    async fn delete_device(&self, device_id: &str) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!("devices/{device_id}"))?;
        let builder = self.add_auth(self.client.delete(url)).await?;
        builder.send().await?;
        Ok(())
    }

    async fn clear_device_share_name(&self, device_id: &str) -> anyhow::Result<()> {
        let url = self.base_url.join(&format!("devices/{device_id}"))?;
        let body = serde_json::to_vec(&serde_json::json!({ "Share": { "Name": "" } }))?;
        let builder = self
            .add_auth(
                self.client
                    .put(url)
                    .header("Content-Type", "application/json")
                    .body(body),
            )
            .await?;
        builder.send().await?;
        Ok(())
    }
}

/// Parsed metadata for a device as returned by the API (name still encrypted).
#[derive(Debug, Clone)]
pub struct RawDeviceInfo {
    pub device_id: String,
    pub volume_id: VolumeId,
    pub share_id: ShareId,
    /// The root folder link ID for the device.
    pub root_link_id: LinkId,
    pub device_type: DeviceType,
    pub create_time: DateTime<Utc>,
    pub last_sync_time: Option<DateTime<Utc>>,
    /// True when the name is stored (in plain text/deprecated form) on the share
    /// rather than encrypted on the root folder node.
    pub has_deprecated_name: bool,
}

impl TryFrom<DeviceListEntry> for RawDeviceInfo {
    type Error = anyhow::Error;

    fn try_from(e: DeviceListEntry) -> anyhow::Result<Self> {
        use chrono::TimeZone;
        Ok(Self {
            device_id: e.device.device_id,
            volume_id: e.device.volume_id,
            share_id: e.share.share_id,
            root_link_id: e.share.link_id,
            device_type: e.device.r#type,
            create_time: Utc
                .timestamp_opt(e.device.create_time, 0)
                .single()
                .ok_or_else(|| anyhow::anyhow!("Invalid create_time"))?,
            last_sync_time: e
                .device
                .last_sync_time
                .and_then(|t| Utc.timestamp_opt(t, 0).single()),
            has_deprecated_name: e.share.name.map(|n| !n.is_empty()).unwrap_or(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(create_time: i64, last_sync_time: Option<i64>, name: Option<&str>) -> DeviceListEntry {
        DeviceListEntry {
            device: DeviceDto {
                device_id: "device".into(),
                volume_id: VolumeId::new("volume".into()),
                r#type: DeviceType::Linux,
                create_time,
                last_sync_time,
            },
            share: DeviceShareDto {
                share_id: ShareId::new("share".into()),
                link_id: LinkId::new("root".into()),
                name: name.map(str::to_owned),
            },
        }
    }

    #[test]
    fn converts_device_api_entry() {
        let device = RawDeviceInfo::try_from(entry(1_700_000_000, None, None)).unwrap();
        assert_eq!(device.device_id, "device");
        assert_eq!(device.volume_id.raw(), "volume");
        assert_eq!(device.share_id.raw(), "share");
        assert_eq!(device.root_link_id.raw(), "root");
        assert_eq!(device.device_type, DeviceType::Linux);
        assert_eq!(device.create_time.timestamp(), 1_700_000_000);
    }

    #[test]
    fn converts_optional_last_sync_time() {
        let device =
            RawDeviceInfo::try_from(entry(1_700_000_000, Some(1_700_000_100), None)).unwrap();
        assert_eq!(
            device.last_sync_time.map(|time| time.timestamp()),
            Some(1_700_000_100)
        );
    }

    #[test]
    fn detects_only_nonempty_deprecated_names() {
        assert!(
            !RawDeviceInfo::try_from(entry(1, None, None))
                .unwrap()
                .has_deprecated_name
        );
        assert!(
            !RawDeviceInfo::try_from(entry(1, None, Some("")))
                .unwrap()
                .has_deprecated_name
        );
        assert!(
            RawDeviceInfo::try_from(entry(1, None, Some("Computer")))
                .unwrap()
                .has_deprecated_name
        );
    }

    #[test]
    fn rejects_invalid_create_time() {
        assert!(RawDeviceInfo::try_from(entry(i64::MAX, None, None)).is_err());
    }
}
