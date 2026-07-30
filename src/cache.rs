use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use crate::config::{CacheConfig, CacheMode};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::fs;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct ResponseCache {
    config: CacheConfig,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct CacheStats {
    pub entries: u64,
    pub bytes: u64,
}

impl ResponseCache {
    pub fn new(config: CacheConfig) -> Result<Self> {
        Ok(Self { config })
    }

    pub fn directory(&self) -> &Path {
        &self.config.directory
    }

    pub fn mode(&self) -> CacheMode {
        self.config.mode
    }

    pub fn key<T: Serialize>(&self, namespace: &str, request: &T) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(namespace.as_bytes());
        hasher.update([0]);
        hasher.update(serde_json::to_vec(request)?);
        Ok(format!("{:x}", hasher.finalize()))
    }

    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        if !self.config.mode.can_read() {
            return Ok(None);
        }
        let path = self.path_for(key);
        let bytes = match fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        match serde_json::from_slice(&bytes) {
            Ok(value) => Ok(Some(value)),
            Err(_) => {
                let _ = fs::remove_file(path).await;
                Ok(None)
            }
        }
    }

    pub async fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        if !self.config.mode.can_write() {
            return Ok(());
        }
        let path = self.path_for(key);
        let parent = path.parent().expect("cache entry has a parent");
        fs::create_dir_all(parent).await?;
        let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".{}.{}.tmp", std::process::id(), counter));
        fs::write(&temp, serde_json::to_vec(value)?).await?;
        if let Err(error) = fs::rename(&temp, &path).await {
            if fs::metadata(&path).await.is_ok() {
                let _ = fs::remove_file(&temp).await;
            } else {
                return Err(error)
                    .with_context(|| format!("commit cache entry {}", path.display()));
            }
        }
        Ok(())
    }

    pub async fn stats(&self) -> Result<CacheStats> {
        scan_cache(&self.config.directory).await
    }

    pub async fn prune(&self, max_megabytes: u64) -> Result<CacheStats> {
        let limit = max_megabytes.saturating_mul(1_048_576);
        let mut entries = cache_entries(&self.config.directory).await?;
        let mut bytes: u64 = entries.iter().map(|entry| entry.bytes).sum();
        entries.sort_by_key(|entry| entry.modified_ms);
        for entry in entries {
            if bytes <= limit {
                break;
            }
            if fs::remove_file(&entry.path).await.is_ok() {
                bytes = bytes.saturating_sub(entry.bytes);
            }
        }
        self.stats().await
    }

    fn path_for(&self, key: &str) -> PathBuf {
        let prefix = key.get(..2).unwrap_or("00");
        self.config
            .directory
            .join(prefix)
            .join(format!("{key}.json"))
    }
}

#[derive(Debug)]
struct CacheEntry {
    path: PathBuf,
    bytes: u64,
    modified_ms: u128,
}

async fn scan_cache(root: &Path) -> Result<CacheStats> {
    let entries = cache_entries(root).await?;
    Ok(CacheStats {
        entries: entries.len() as u64,
        bytes: entries.iter().map(|entry| entry.bytes).sum(),
    })
}

async fn cache_entries(root: &Path) -> Result<Vec<CacheEntry>> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut found = Vec::new();
        if !root.exists() {
            return Ok(found);
        }
        for prefix in std::fs::read_dir(root)? {
            let prefix = prefix?;
            if !prefix.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(prefix.path())? {
                let entry = entry?;
                let metadata = entry.metadata()?;
                if !metadata.is_file() {
                    continue;
                }
                let modified_ms = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis())
                    .unwrap_or_default();
                found.push(CacheEntry {
                    path: entry.path(),
                    bytes: metadata.len(),
                    modified_ms,
                });
            }
        }
        Ok::<_, std::io::Error>(found)
    })
    .await?
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_cached_json() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ResponseCache::new(CacheConfig {
            mode: CacheMode::ReadWrite,
            directory: temp.path().into(),
            ..Default::default()
        })
        .unwrap();
        let key = cache.key("test", &serde_json::json!({"a": 1})).unwrap();
        cache
            .put(&key, &serde_json::json!({"answer": 42}))
            .await
            .unwrap();
        let value: serde_json::Value = cache.get(&key).await.unwrap().unwrap();
        assert_eq!(value["answer"], 42);
        assert_eq!(cache.stats().await.unwrap().entries, 1);
    }
}
