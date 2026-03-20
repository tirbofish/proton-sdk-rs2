use futures::stream::BoxStream;
use parking_lot::RwLock;
use proton_sdk_rs2::cache::CacheRepository;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

#[derive(serde::Deserialize, serde::Serialize)]
struct FileCacheInner {
    entries: HashMap<String, String>,
    key_to_tags: HashMap<String, HashSet<String>>,
    tag_to_keys: HashMap<String, HashSet<String>>,
}

pub struct FileCacheRepository {
    path: PathBuf,
    inner: RwLock<FileCacheInner>,
}

impl FileCacheRepository {
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let inner = if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            serde_json::from_str(&content)?
        } else {
            FileCacheInner {
                entries: Default::default(),
                key_to_tags: Default::default(),
                tag_to_keys: Default::default(),
            }
        };

        Ok(Self {
            path,
            inner: RwLock::new(inner),
        })
    }

    fn persist(&self) -> anyhow::Result<()> {
        let inner = self.inner.read();
        let content = serde_json::to_string(&*inner)?;
        std::fs::write(&self.path, &content)?;
        crate::fs_permissions::set_restricted_permissions(&self.path)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl CacheRepository for FileCacheRepository {
    async fn set(&self, key: &str, value: String, tags: Vec<String>) -> anyhow::Result<()> {
        let mut inner = self.inner.write();

        if let Some(old_tags) = inner.key_to_tags.remove(key) {
            for tag in old_tags {
                if let Some(keys) = inner.tag_to_keys.get_mut(&tag) {
                    keys.remove(key);
                }
            }
        }

        inner.entries.insert(key.to_string(), value);

        let new_tags: HashSet<String> = tags.into_iter().collect();
        inner.key_to_tags.insert(key.to_string(), new_tags.clone());

        for tag in new_tags {
            inner
                .tag_to_keys
                .entry(tag)
                .or_insert_with(HashSet::new)
                .insert(key.to_string());
        }

        drop(inner);
        self.persist()?;
        Ok(())
    }

    async fn remove(&self, key: &str) -> anyhow::Result<()> {
        let mut inner = self.inner.write();
        inner.entries.remove(key);
        if let Some(tags) = inner.key_to_tags.remove(key) {
            for tag in tags {
                if let Some(keys) = inner.tag_to_keys.get_mut(&tag) {
                    keys.remove(key);
                }
            }
        }
        drop(inner);
        self.persist()?;
        Ok(())
    }

    async fn remove_by_tag(&self, tag: &str) -> anyhow::Result<()> {
        let keys: Vec<String> = {
            let mut inner = self.inner.write();
            inner
                .tag_to_keys
                .remove(tag)
                .map(|k| k.into_iter().collect())
                .unwrap_or_default()
        };

        for key in keys {
            self.remove(&key).await?;
        }

        Ok(())
    }

    async fn clear(&self) -> anyhow::Result<()> {
        let mut inner = self.inner.write();
        inner.entries.clear();
        inner.key_to_tags.clear();
        inner.tag_to_keys.clear();
        drop(inner);
        self.persist()?;
        Ok(())
    }

    async fn try_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self.inner.read().entries.get(key).cloned())
    }

    fn get_by_tags(&self, tags: Vec<String>) -> BoxStream<'_, anyhow::Result<(String, String)>> {
        use futures::stream::{self, StreamExt};

        let results = {
            let inner = self.inner.read();
            let mut keys_set = HashSet::new();
            for tag in &tags {
                if let Some(keys) = inner.tag_to_keys.get(tag) {
                    keys_set.extend(keys.iter().cloned());
                }
            }

            keys_set
                .into_iter()
                .filter_map(|key| {
                    inner
                        .entries
                        .get(&key)
                        .map(|value| Ok((key.clone(), value.clone())))
                })
                .collect::<Vec<_>>()
        };

        stream::iter(results).boxed()
    }
}