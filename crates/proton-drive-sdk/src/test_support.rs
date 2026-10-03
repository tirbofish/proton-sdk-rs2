//! In-memory client components for tests that must not contact a live account.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use proton_sdk_rs2::cache::InMemoryCacheRepository;
use proton_sdk_rs2::client::{AlwaysDisabledFeatureFlagProvider, Telemetry};
use proton_sdk_rs2::protobuf::Address;

use crate::account::{AccountClient, AddressId};
use crate::api::DefaultDriveApiClientsFactory;
use crate::cache::entity::{DefaultDriveEntityCache, DriveEntityCache};
use crate::cache::secret::DefaultDriveSecretCache;
use crate::client::ProtonDriveClient;
use crate::share::{Share, ShareId};

#[derive(Default)]
pub(crate) struct RecordingTelemetry(pub Mutex<Vec<(String, serde_json::Value)>>);

#[async_trait]
impl Telemetry for RecordingTelemetry {
    async fn record_metric(&self, name: String, payload: Option<Vec<u8>>) {
        self.0
            .lock()
            .push((name, serde_json::from_slice(&payload.unwrap()).unwrap()));
    }
}

struct TestAccount;

#[async_trait]
impl AccountClient for TestAccount {
    async fn get_address(&self, id: &AddressId) -> anyhow::Result<Address> {
        assert_eq!(id.raw(), "my-files-member");
        Ok(Address {
            address_id: id.raw().into(),
            email_address: "member@example.test".into(),
            ..Default::default()
        })
    }

    async fn get_default_address(&self) -> anyhow::Result<Address> {
        panic!("use the My Files member, not the default address")
    }

    async fn get_address_primary_private_key(
        &self,
        _: &AddressId,
    ) -> anyhow::Result<proton_rpgp::PrivateKey> {
        panic!("unexpected key lookup")
    }

    async fn get_address_private_keys(
        &self,
        _: &AddressId,
    ) -> anyhow::Result<Vec<proton_rpgp::PrivateKey>> {
        panic!("unexpected key lookup")
    }

    async fn get_address_public_keys(
        &self,
        _: &str,
    ) -> anyhow::Result<Vec<proton_rpgp::PublicKey>> {
        Ok(Vec::new())
    }

    async fn get_user_keys(&self) -> anyhow::Result<Vec<proton_rpgp::PrivateKey>> {
        panic!("unexpected user key lookup")
    }

    async fn get_user_storage_info(&self) -> anyhow::Result<(i64, i64)> {
        panic!("unexpected quota lookup")
    }
}

pub(crate) async fn client() -> (ProtonDriveClient, Arc<RecordingTelemetry>) {
    let entities = Arc::new(DefaultDriveEntityCache::new(Arc::new(
        InMemoryCacheRepository::new(),
    )));
    let share_id = ShareId::new("my-files-share".into());
    entities
        .set_my_files_share_id(share_id.clone())
        .await
        .unwrap();
    entities
        .set_share(Share {
            id: share_id,
            root_folder_id: crate::node::NodeUid::from_parts("volume", "root"),
            membership_address_id: AddressId::new("my-files-member".into()),
            share_type: crate::api::share::ShareType::Main,
        })
        .await
        .unwrap();
    let telemetry = Arc::new(RecordingTelemetry::default());
    let client = ProtonDriveClient::from_http_client_factory_with_drive_api_clients_factory(
        Arc::new(TestAccount),
        entities,
        Arc::new(DefaultDriveSecretCache::new(Arc::new(
            InMemoryCacheRepository::new(),
        ))),
        Arc::new(AlwaysDisabledFeatureFlagProvider),
        telemetry.clone(),
        Arc::new(DefaultDriveApiClientsFactory),
        None,
    )
    .unwrap();
    (client, telemetry)
}
