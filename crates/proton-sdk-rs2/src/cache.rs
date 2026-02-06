use std::collections::HashSet;

use dashmap::DashMap;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

#[async_trait::async_trait]
pub trait CacheRepositoryTrait: Send + Sync {
    async fn set(
        &self,
        key: &str,
        value: String,
        tags: Vec<String>,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<()>;

    async fn remove(
        &self,
        key: &str,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<()>;

    async fn remove_by_tag(
        &self,
        tag: &str,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<()>;

    async fn clear(&self) -> anyhow::Result<()>;

    async fn try_get(
        &self,
        key: &str,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<Option<String>>;

    fn get_by_tags(
        &self,
        tags: Vec<String>,
        cancellation_token: CancellationToken,
    ) -> BoxStream<'_, anyhow::Result<(String, String)>>;
}

pub struct InMemoryCacheRepository {
    entries: DashMap<String, String>,
    key_to_tags: DashMap<String, HashSet<String>>,
    tag_to_keys: DashMap<String, HashSet<String>>,
}

impl InMemoryCacheRepository {
    pub fn new() -> Self {
        Self {
            entries: Default::default(),
            key_to_tags: Default::default(),
            tag_to_keys: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl CacheRepositoryTrait for InMemoryCacheRepository {
    async fn set(
        &self,
        key: &str,
        value: String,
        tags: Vec<String>,
        _cancellation_token: CancellationToken,
    ) -> anyhow::Result<()> {
        // Clear existing tags for this key
        if let Some((_, old_tags)) = self.key_to_tags.remove(key) {
            for tag in old_tags {
                if let Some(mut keys) = self.tag_to_keys.get_mut(&tag) {
                    keys.remove(key);
                }
            }
        }

        self.entries.insert(key.to_string(), value);

        let new_tags: HashSet<String> = tags.into_iter().collect();
        self.key_to_tags.insert(key.to_string(), new_tags.clone());

        for tag in new_tags {
            self.tag_to_keys
                .entry(tag)
                .or_insert_with(HashSet::new)
                .insert(key.to_string());
        }

        Ok(())
    }

    async fn remove(
        &self,
        key: &str,
        _cancellation_token: CancellationToken,
    ) -> anyhow::Result<()> {
        self.entries.remove(key);

        if let Some((_, tags)) = self.key_to_tags.remove(key) {
            for tag in tags {
                if let Some(mut keys) = self.tag_to_keys.get_mut(&tag) {
                    keys.remove(key);
                }
            }
        }

        Ok(())
    }

    async fn remove_by_tag(
        &self,
        tag: &str,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<()> {
        if let Some((_, keys)) = self.tag_to_keys.remove(tag) {
            for key in keys {
                self.remove(&key, cancellation_token.clone()).await?;
            }
        }

        Ok(())
    }

    async fn clear(&self) -> anyhow::Result<()> {
        self.entries.clear();
        self.key_to_tags.clear();
        self.tag_to_keys.clear();
        Ok(())
    }

    async fn try_get(
        &self,
        key: &str,
        _cancellation_token: CancellationToken,
    ) -> anyhow::Result<Option<String>> {
        Ok(self.entries.get(key).map(|v| v.clone()))
    }

    fn get_by_tags(
        &self,
        tags: Vec<String>,
        _cancellation_token: CancellationToken,
    ) -> BoxStream<'_, anyhow::Result<(String, String)>> {
        use futures::stream::{self, StreamExt};

        let mut keys_set = HashSet::new();

        for tag in tags {
            if let Some(keys) = self.tag_to_keys.get(&tag) {
                keys_set.extend(keys.iter().cloned());
            }
        }

        let results: Vec<_> = keys_set
            .into_iter()
            .filter_map(|key| {
                self.entries
                    .get(&key)
                    .map(|value| Ok((key.clone(), value.clone())))
            })
            .collect();

        stream::iter(results).boxed()
    }
}
