use chrono::Duration as ChronoDuration;
use dashmap::DashMap;
use tokio::sync::Mutex;
use std::collections::{BinaryHeap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{Ordering, AtomicUsize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use twox_hash::XxHash64;
use tracing::{debug, error, info};

use bincode::{serialize, deserialize};
use tokio::fs::{read, write};
use crate::store::calod_data::{CacheEntry, EvictionCandidate};

use super::calod_data::DataType;
use super::config::CacheConfig;
use super::metrics::CalodMetrics;

#[derive(Debug, Error, Clone)]
pub enum CacheError {
    #[error("Key `{0}` not found")]
    KeyNotFound(String),
    #[error("Key `{0}` has expired")]
    KeyExpired(String),
    #[error("Invalid command arguments: {0}")]
    InvalidCommandArguments(String),
    #[error("Command not implemented: {0}")]
    CommandNotImplemented(String),
    #[error("Internal cache error: {0}")]
    InternalError(String),
}


#[derive(Debug)]
pub enum PersistenceError {
    IoError(std::io::Error),
    SerializationError(bincode::Error),
}

type ShardIndex = usize;

pub struct ShardedStore {
    shards: Vec<Arc<CalodShard>>,
    metrics: Arc<CalodMetrics>,
    config: CacheConfig
}

#[async_trait::async_trait]
pub trait CachePersistence {
    async fn save(&self, path: &str) -> Result<(), PersistenceError>;
    async fn load(&self, path: &str) -> Result<(), PersistenceError>;
}

impl ShardedStore {
    pub fn new(config: CacheConfig) -> Self {
        let metrics = Arc::new(CalodMetrics::default());
        let per_shard_cap = (config.capacity + config.shards - 1) / config.shards;

        let shards = (0..config.shards).map(|_| Arc::new(CalodShard::new(per_shard_cap, metrics.clone()))).collect();

        Self {
            shards,
            config,
            metrics
        }
    }

    pub async fn ping(&self) -> String {
        "+PONG\r\n".to_string()
    }

    pub async fn info(&self, _section: Option<&str>) -> String {
        let metrics_report = self.metrics.report();
        format!("{}\r\n", metrics_report)
    }

    #[inline]
    fn get_shard_index<K: Hash + ?Sized>(&self, key: &K) -> ShardIndex where K:AsRef<[u8]> {
        let mut hasher = XxHash64::default();
        key.hash(&mut hasher);
        (hasher.finish() % self.shards.len() as u64) as usize
    }

    pub async fn get(&self, key: &str) -> Result<DataType, CacheError> {
        let start = Instant::now();
        let shard_idx = self.get_shard_index(key);
        let result = self.shards[shard_idx].get(key).await;

        if self.config.metrics_enabled {
            let latency = start.elapsed();
            self.metrics.record_read(latency);

            match &result {
                Ok(_) => self.metrics.record_hit(),
                Err(CacheError::KeyNotFound(_)) => self.metrics.record_miss(),
                _ => (),
            }
        }

        result
    }

    pub async fn set(&self, key: String, value: DataType, ttl: Option<ChronoDuration>) -> Option<DataType> {
        let start = Instant::now();
        let shard_idx = self.get_shard_index(&key);
        let result = self.shards[shard_idx].set(key, value, ttl).await;

        if self.config.metrics_enabled {
            let latency = start.elapsed();
            self.metrics.record_write(latency);
        }

        result
    }

    pub async fn delete(&self, key: &str) -> bool {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].delete(key).await
    }

    pub async fn exists(&self, key: &str) -> bool {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].exists(key).await
    }

    pub async fn keys(&self, pattern: &str) -> Vec<String> {
        let mut all_keys = Vec::new();
        for shard in &self.shards {
            all_keys.extend(shard.keys(pattern).await);
        }
        all_keys
    }

    pub async fn expire(&self, key: &str, seconds: u64) -> Result<bool, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].exipre(key, seconds).await
    }

    pub async fn ttl(&self, key: &str) -> Result<Option<i64>, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].ttl(key).await
    }

    pub async fn persist(&self, key: &str) -> Result<bool, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].persist(key).await
    }

    // pub async fn atomic_incr(&self, key: &str) -> Result<i64, CacheError> {
    //     let shard_idx = self.get_shard_index(key);
    //     let shard = &self.shards[shard_idx];

    //     let mut entry = shard.data.entry(key.to_string()).or_insert(CacheEntry::new(DataType::Int(0), None));

    //     match &mut entry.value {
    //         DataType::Int(n) => {
    //             *n += 1;
    //             Ok(*n)
    //         }
    //         _ => Err(CacheError::KeyNotFound(key.to_string())),
    //     }
    // }

    // pub async fn hset(&self, key: &str, field: &str, value: String) -> bool {
    //     let shard_idx = self.get_shard_index(key);
    //     let shard = &self.shards[shard_idx];

    //     let mut hash = shard.data.entry(key.to_string()).or_insert(DashMap::new());

    //     hash.insert(field.to_string(), value).is_none()
    // }

    pub fn metrics(&self) -> String {
        self.metrics.report()
    }
}

#[async_trait::async_trait]
impl CachePersistence for ShardedStore {
    async fn save(&self, path: &str) -> Result<(), PersistenceError> {
        let mut handles = Vec::new();

        for (i, shard) in self.shards.iter().enumerate() {
            let shard = Arc::clone(shard);
            let path = format!("{}/shard_{}.bin", path, i);
            handles.push(tokio::spawn(async move {
                shard.save(&path).await
            }));
        }

        for handle in handles {
            handle.await.map_err(|e| PersistenceError::IoError(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Join error: {}", e)
            )))??;
        }

        Ok(())
    }
    
    async fn load(&self, path: &str) -> Result<(), PersistenceError> {
        for (i, shard) in self.shards.iter().enumerate() {
            let path = format!("{}/shard_{}.bin", path, i);
            shard.load(&path).await?;
        }
        Ok(())
    }
}

struct CalodShard {
    data: DashMap<String, CacheEntry>,
    lru: Mutex<VecDeque<String>>,
    capacity: AtomicUsize,
    size: AtomicUsize,
    metrics: Arc<CalodMetrics>,
}

impl CalodShard {
    fn new(capacity: usize, metrics: Arc<CalodMetrics>) -> Self {
        Self {
            data: DashMap::new(),
            lru: Mutex::new(VecDeque::new()),
            capacity: AtomicUsize::new(capacity),
            size: AtomicUsize::new(0),
            metrics,
        }
    }

    async fn get(&self, key: &str) -> Result<DataType, CacheError> {
        let entry_ref = self.data.get(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_ref.value();
        if entry.is_expired() {
            drop(entry_ref);
            self.data.remove(key);
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            return Err(CacheError::KeyExpired(key.to_string()));
        }

        self.touch_key(key).await;
        Ok(entry.value.clone())
    }

    async fn set(&self, key: String, value: DataType, ttl: Option<ChronoDuration>) -> Option<DataType> {
        if self.size.load(Ordering::Relaxed) >= self.capacity.load(Ordering::Relaxed) {
            self.evict().await;
        }
        let entry = CacheEntry::new(value, ttl);
        let entry_size = entry.size();
        let old_entry = self.data.insert(key.clone(), entry);

        self.touch_key(&key).await;
        self.size.fetch_add(1, Ordering::Relaxed);

        if let Some(old) = &old_entry {
            self.metrics.total_data_size.fetch_sub(old.size() as u64, Ordering::Relaxed);
        }
        self.metrics.total_data_size.fetch_add(entry_size as u64, Ordering::Relaxed);

        old_entry.map(|e| e.value)
    }

    async fn exists(&self, key: &str) -> bool {
        if let Some(entry_ref) = self.data.get(key) {
            if !entry_ref.is_expired() {
                return true;
            } else {
                drop(entry_ref);
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
                return false;
            }
        }
        false
    }

    async fn keys(&self, pattern: &str) -> Vec<String> {
        self.data.iter().filter_map(|entry| {
            if entry.key().contains(pattern) && !entry.value().is_expired() {
                Some(entry.key().clone())
            } else {
                None
            }
        }).collect()
    }

    async fn exipre(&self, key: &str, seconds: u64) -> Result<bool, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        entry_writer.value_mut().expire_in(Duration::from_secs(seconds));
        Ok(true)
    }

    async fn ttl(&self, key: &str) -> Result<Option<i64>, CacheError> {
        let entry_ref = self.data.get(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_ref.value();
        if entry.is_expired() {
            drop(entry_ref);        // Drop read guard before deletion
            self.data.remove(key);
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            return Err(CacheError::KeyExpired(key.to_string()));
        }

        Ok(entry.ttl().map(|expiry| {
            expiry.num_seconds()
        }))
    }

    async fn persist(&self, key: &str) -> Result<bool, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        entry_writer.value_mut().persist();
        Ok(true)
    }

    async fn save(&self, path: &str) -> Result<(), PersistenceError> {
        let data = self.data.iter().map(|entry| (entry.key().clone(), entry.value().clone())).collect::<Vec<_>>();

        let encoded = serialize(&data).map_err(PersistenceError::SerializationError)?;

        write(path, &encoded).await.map_err(PersistenceError::IoError)?;

        Ok(())
    }

    async fn load(&self, path: &str) -> Result<(), PersistenceError> {
        let encoded = read(path).await.map_err(PersistenceError::IoError)?;
        let data: Vec<(String, CacheEntry)> = deserialize(&encoded).map_err(PersistenceError::SerializationError)?;

        for (key, entry) in data {
            self.data.insert(key, entry);
        }

        Ok(())
    }

    async fn delete(&self, key: &str) -> bool {
        if self.data.remove(key).is_some() {
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            true
        } else {
            false
        }
    }

    async fn touch_key(&self, key: &str) {
        let mut lru = self.lru.lock().await;
        lru.retain(|k| k != key);
        lru.push_front(key.to_string());
    }

    async fn remove_from_lru(&self, key: &str) {
        let mut lru = self.lru.lock().await;
        lru.retain(|k| k != key);
    }

    async fn evict(&self) {
        let mut candidates = BinaryHeap::new();
        let lru = self.lru.lock().await;

        for key in lru.iter().rev().take(5) {
            if let Some(entry) = self.data.get(key) {
                let score = entry.eviction_score();
                candidates.push(EvictionCandidate {
                    key: key.clone(),
                    score,
                });
            }
        }

        if let Some(candidate) = candidates.pop() {
            if self.data.remove(&candidate.key).is_some() {
                self.size.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}


// #[tokio::main]
// async fn main() -> Result<(), Box<dyn std::error::Error>> {
//     let config = CacheConfig {
//         shards: 32,
//         capacity: 1_000_000,
//         ..Default::default()
//     };

//     let cache = ShardedStore::new(config);

//     cache.set("key1".to_string(), DataType::String("Value".into()), None).await;
//     // Get metrics
//     println!("{}", cache.metrics());
    
//     // Save state
//     cache.save("./cache_backup").await?;
     
//     // Load state
//     cache.load("./cache_backup").await?;
 
//     Ok(())
// }