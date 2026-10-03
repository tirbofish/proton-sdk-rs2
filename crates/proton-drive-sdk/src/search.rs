//! Experimental search integration. Applications supply a runtime-specific provider.
//!
//! The public upstream mirror does not include the incubating search engine.
//! As in the JS SDK, this interface currently exposes only `enable`.

use std::sync::Arc;

use async_trait::async_trait;

#[async_trait]
pub trait ProtonDriveSearchClient: Send + Sync {
    async fn enable(&self) -> anyhow::Result<()>;
}

#[async_trait]
pub trait SearchServiceProvider: Send + Sync {
    async fn start(
        &self,
        sdk_version: &str,
        address_id: &str,
    ) -> anyhow::Result<Arc<dyn ProtonDriveSearchClient>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct SearchClient(Arc<AtomicBool>);

    #[async_trait]
    impl ProtonDriveSearchClient for SearchClient {
        async fn enable(&self) -> anyhow::Result<()> {
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct Provider(Arc<AtomicBool>);

    #[async_trait]
    impl SearchServiceProvider for Provider {
        async fn start(
            &self,
            version: &str,
            address_id: &str,
        ) -> anyhow::Result<Arc<dyn ProtonDriveSearchClient>> {
            assert_eq!(version, env!("CARGO_PKG_VERSION"));
            assert_eq!(address_id, "my-files-member");
            Ok(Arc::new(SearchClient(self.0.clone())))
        }
    }

    #[tokio::test]
    async fn search_requires_a_provider_and_passes_version_and_member_address() {
        let (drive, _) = crate::test_support::client().await;
        assert!(
            drive
                .init_search()
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("provider not available")
        );
        let enabled = Arc::new(AtomicBool::new(false));
        let provider = Provider(enabled.clone());
        let client = drive
            .with_search_service_provider(Arc::new(provider))
            .init_search()
            .await
            .unwrap();
        assert!(!enabled.load(Ordering::SeqCst));
        client.enable().await.unwrap();
        assert!(enabled.load(Ordering::SeqCst));
    }
}
