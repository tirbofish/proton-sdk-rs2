use std::collections::HashSet;

use dashmap::DashMap;
use futures::stream::BoxStream;

#[async_trait::async_trait]
pub trait CacheRepository: Send + Sync {
    async fn set(&self, key: &str, value: String, tags: Vec<String>) -> anyhow::Result<()>;
    async fn remove(&self, key: &str) -> anyhow::Result<()>;
    async fn remove_by_tag(&self, tag: &str) -> anyhow::Result<()>;
    async fn clear(&self) -> anyhow::Result<()>;
    async fn try_get(&self, key: &str) -> anyhow::Result<Option<String>>;
    fn get_by_tags(&self, tags: Vec<String>) -> BoxStream<'_, anyhow::Result<(String, String)>>;
}

/// An in-memory cache that loses all data when the process exits.
///
/// This is the default cache used when no persistent storage is configured.
/// Suitable for short-lived sessions or testing.
///
/// # Issue
/// This cache is not suitable fo resumed sessions. If you call
/// [`ProtonAPISession::from_stored_credentials`] with an `InMemoryCacheRepository`,
/// key material (passphrases, session keys) will be missing and decryption will fail.
///
/// For persistent sessions, use `SqliteCacheRepository` from `proton_drive_sdk::cache::sqlite` (it supports
/// in memory as well).
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
impl CacheRepository for InMemoryCacheRepository {
    async fn set(&self, key: &str, value: String, tags: Vec<String>) -> anyhow::Result<()> {
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

    async fn remove(&self, key: &str) -> anyhow::Result<()> {
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

    async fn remove_by_tag(&self, tag: &str) -> anyhow::Result<()> {
        if let Some((_, keys)) = self.tag_to_keys.remove(tag) {
            for key in keys {
                self.remove(&key).await?;
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

    async fn try_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self.entries.get(key).map(|v| v.clone()))
    }

    fn get_by_tags(&self, tags: Vec<String>) -> BoxStream<'_, anyhow::Result<(String, String)>> {
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
