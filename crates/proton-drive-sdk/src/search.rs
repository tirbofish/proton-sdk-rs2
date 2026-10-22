//! Experimental search integration. Applications supply a runtime-specific provider.
//!
//! The public upstream mirror does not include the incubating search engine, so
//! no provider ships with the SDK. This module mirrors upstream's experimental
//! interface: the search client facade, its query and status types, and the
//! capabilities the SDK client hands to the search module. Cancellation is by
//! dropping the returned future or stream; progress made so far is kept.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;

use crate::node::{DegradedNode, Node, NodeUid};
use crate::utils::PotentialObject;

/// Logical set of nodes a device indexes and searches. Not 1:1 with the
/// indexes in storage: the module may split one logical index into several.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchIndex {
    /// Nodes selected according to recent access.
    RecentNodes,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SearchQuery {
    Text(String),
    Structured(StructuredSearchQuery),
}

/// Text matched against node names, narrowed by filters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StructuredSearchQuery {
    pub name: String,
    pub parent_uid: Option<NodeUid>,
    /// Wildcards allowed, e.g. `image/*`.
    pub media_type: Option<String>,
    pub created_by: Option<String>,
    pub owned_by: Option<String>,
    pub creation_time: Option<DateRange>,
    pub modification_time: Option<DateRange>,
    pub last_access_time: Option<DateRange>,
    /// In bytes.
    pub size: Option<SizeRange>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DateRange {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SizeRange {
    pub from: Option<u64>,
    pub to: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub node_uid: NodeUid,
    /// Meaningful only within the same query.
    pub score: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunIndexingOptions {
    /// Maximum local storage the indexes may use, in bytes. Defaults to 25 MB.
    pub storage_quota: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunSchedule {
    /// Recommended delay before calling the same method again; zero means as soon as practical.
    pub recommended_next_call_after: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    /// Indexing has not been activated yet.
    NoIndex,
    IndexingInProgress,
    IndexingCompleted,
    IndexingFailed,
    /// The index is available, but some nodes failed to be indexed.
    IndexIncompleteDueToFailure,
    /// The index reached its storage quota.
    IndexIncompleteDueToSpaceLimit,
    /// Some extracts are incompatible and an upgrade is required.
    IndexIncompleteDueToIncompatibleExtracts,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchStatus {
    /// `true` when all known nodes have extracted data.
    pub is_data_extraction_queue_empty: bool,
    /// `true` when nodes that should be indexed are still missing extracted data.
    pub is_data_extraction_for_local_indexes_needed: bool,
    pub indexes_statuses: Vec<(SearchIndex, IndexStatus)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchIndexDebugInfo {
    /// Diagnostic only; not 1:1 with [`SearchIndex`].
    pub index_name: String,
    pub index_size_bytes: u64,
    pub indexed_nodes_count: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchDebugInfo {
    pub indexes: Vec<SearchIndexDebugInfo>,
    pub extracting_nodes_count: u64,
}

/// The search module's public facade. Experimental: the API will change as the
/// drive search module grows.
#[async_trait]
pub trait ProtonDriveSearchClient: Send + Sync {
    /// Builds and updates the logical indexes until no indexing work remains.
    async fn run_indexing(
        &self,
        indexes: &[SearchIndex],
        options: RunIndexingOptions,
    ) -> anyhow::Result<RunSchedule>;

    /// Extracts the nodes the backend's extraction queue hands out and uploads
    /// the extracts, until the queue is empty or throttles this client.
    async fn run_extraction(&self) -> anyhow::Result<RunSchedule>;

    /// Streams matches from `index` as they are found.
    fn query(
        &self,
        index: SearchIndex,
        query: SearchQuery,
    ) -> BoxStream<'_, anyhow::Result<SearchResult>>;

    /// Removes deleted nodes from every local index.
    async fn nodes_deleted(&self, node_uids: Vec<NodeUid>) -> anyhow::Result<()>;

    async fn get_status(&self) -> anyhow::Result<SearchStatus>;

    async fn get_debug_info(&self) -> anyhow::Result<SearchDebugInfo>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchApiMethod {
    Get,
    Post,
    Put,
}

/// What the search module needs from the SDK client: the nodes, and the
/// authenticated Drive API.
#[async_trait]
pub trait SearchSdkCapabilities: Send + Sync {
    /// Nodes for the UIDs; missing or failing ones are left out.
    async fn load_nodes(
        &self,
        node_uids: Vec<NodeUid>,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>>;

    /// Calls the Drive API with a path relative to its base URL.
    ///
    /// Fails when the request fails or the API answers with an error code.
    async fn api_request(
        &self,
        method: SearchApiMethod,
        path: &str,
        json: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value>;
}

/// Provides a search service implementation tailored to its runtime.
#[async_trait]
pub trait SearchServiceProvider: Send + Sync {
    async fn start(
        &self,
        sdk_version: &str,
        address_id: &str,
        sdk: Arc<dyn SearchSdkCapabilities>,
    ) -> anyhow::Result<Arc<dyn ProtonDriveSearchClient>>;
}

/// The SDK client's side of [`SearchSdkCapabilities`].
pub(crate) struct ClientSearchCapabilities {
    pub(crate) client: crate::client::ProtonDriveClient,
}

#[async_trait]
impl SearchSdkCapabilities for ClientSearchCapabilities {
    async fn load_nodes(
        &self,
        node_uids: Vec<NodeUid>,
    ) -> anyhow::Result<Vec<PotentialObject<Node, DegradedNode>>> {
        use futures::StreamExt;
        Ok(futures::stream::iter(node_uids)
            .map(|uid| self.client.get_node(uid))
            .buffered(10)
            .filter_map(|node| async move { node.ok() })
            .collect()
            .await)
    }

    async fn api_request(
        &self,
        method: SearchApiMethod,
        path: &str,
        json: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        self.client.raw_api_request(method, path, json).await
    }
}

/// Fails on a non-success HTTP status or API code, otherwise returns the JSON body.
pub(crate) fn parse_search_api_response(
    status: reqwest::StatusCode,
    body: &str,
) -> anyhow::Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| anyhow::anyhow!("HTTP {status}: invalid JSON ({error}): {body}"))?;
    let code = value.get("Code").and_then(serde_json::Value::as_u64);
    if !status.is_success() || code.is_some_and(|code| code != 1000 && code != 1001) {
        let message = value
            .get("Error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        anyhow::bail!("API error {} (HTTP {status}): {message}", code.unwrap_or(0));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct SearchClient;

    #[async_trait]
    impl ProtonDriveSearchClient for SearchClient {
        async fn run_indexing(
            &self,
            indexes: &[SearchIndex],
            _: RunIndexingOptions,
        ) -> anyhow::Result<RunSchedule> {
            assert_eq!(indexes, [SearchIndex::RecentNodes]);
            Ok(RunSchedule {
                recommended_next_call_after: Duration::ZERO,
            })
        }

        async fn run_extraction(&self) -> anyhow::Result<RunSchedule> {
            unimplemented!()
        }

        fn query(
            &self,
            _: SearchIndex,
            _: SearchQuery,
        ) -> BoxStream<'_, anyhow::Result<SearchResult>> {
            unimplemented!()
        }

        async fn nodes_deleted(&self, _: Vec<NodeUid>) -> anyhow::Result<()> {
            unimplemented!()
        }

        async fn get_status(&self) -> anyhow::Result<SearchStatus> {
            unimplemented!()
        }

        async fn get_debug_info(&self) -> anyhow::Result<SearchDebugInfo> {
            unimplemented!()
        }
    }

    struct Provider {
        starts: AtomicUsize,
        fail_first: bool,
    }

    #[async_trait]
    impl SearchServiceProvider for Provider {
        async fn start(
            &self,
            version: &str,
            address_id: &str,
            _: Arc<dyn SearchSdkCapabilities>,
        ) -> anyhow::Result<Arc<dyn ProtonDriveSearchClient>> {
            assert_eq!(version, env!("CARGO_PKG_VERSION"));
            assert_eq!(address_id, "my-files-member");
            if self.starts.fetch_add(1, Ordering::SeqCst) == 0 && self.fail_first {
                anyhow::bail!("engine unavailable");
            }
            Ok(Arc::new(SearchClient))
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
        let client = drive
            .with_search_service_provider(Arc::new(Provider {
                starts: AtomicUsize::new(0),
                fail_first: false,
            }))
            .init_search()
            .await
            .unwrap();
        let schedule = client
            .run_indexing(&[SearchIndex::RecentNodes], RunIndexingOptions::default())
            .await
            .unwrap();
        assert_eq!(schedule.recommended_next_call_after, Duration::ZERO);
    }

    #[tokio::test]
    async fn search_starts_once_per_client_and_retries_a_failed_start() {
        let (drive, _) = crate::test_support::client().await;
        let provider = Arc::new(Provider {
            starts: AtomicUsize::new(0),
            fail_first: true,
        });
        let drive = drive.with_search_service_provider(provider.clone());

        assert!(drive.init_search().await.is_err());
        let first = drive.init_search().await.unwrap();
        let second = drive.init_search().await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(provider.starts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn search_api_responses_fail_on_error_codes_and_statuses() {
        let ok = reqwest::StatusCode::OK;
        assert_eq!(
            parse_search_api_response(ok, r#"{"Code":1000,"Items":[]}"#).unwrap()["Items"],
            serde_json::json!([])
        );
        assert!(parse_search_api_response(ok, r#"{"Code":2501,"Error":"gone"}"#).is_err());
        assert!(
            parse_search_api_response(reqwest::StatusCode::BAD_GATEWAY, r#"{"Code":1000}"#)
                .is_err()
        );
        assert!(parse_search_api_response(ok, "<html>").is_err());
    }
}
