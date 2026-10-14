use dashmap::DashMap;
use dashmap::DashSet;
use lru::LruCache;
use serde_json::{Error as SerdeJsonError, Value as JsonValue};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::timeout;
use tracing::{error, trace};

use crate::store::calod_data::{CacheEntry, EvictionCandidate};
use crate::store::command_options::{InsertOption, SetExpireOption, SetOption};
use bincode::{deserialize, serialize};
use tokio::fs::{read, write};

use super::calod_data::DataType;
use super::error::{CacheError, PersistenceError};
use super::helpers::json_helpers::{
    json_type_to_string, jsonpath_arrappend, jsonpath_arrindex, jsonpath_arrinsert,
    jsonpath_arrlen, jsonpath_arrpop, jsonpath_arrtrim, jsonpath_del, jsonpath_get,
    jsonpath_numincrby, jsonpath_objkeys, jsonpath_objlen, jsonpath_objset, jsonpath_set,
    jsonpath_strappend,
};
use super::metrics::CalodMetrics;
use crate::store::geo;
use crate::store::hll::Hll;
use crate::store::sorted_set::{parse_lex_bound, parse_score_bound, ScoreBound, SortedSetData};
use crate::store::stream::{self, ClaimOptions, StreamData, StreamId};

static OK_RESPONSE: &str = "OK";
static NONE_TYPE: &str = "none";

fn glob_matches(pattern: &str, text: &str) -> bool {
    fn rec(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            b'*' => rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..])),
            b'?' => !t.is_empty() && rec(&p[1..], &t[1..]),
            ch => !t.is_empty() && ch == t[0] && rec(&p[1..], &t[1..]),
        }
    }

    rec(pattern.as_bytes(), text.as_bytes())
}

/// A single shard of the Calod in‑memory data store.
///
/// Each `CalodShard` manages a partition of the global keyspace. It provides:
///
/// * Concurrent access to heterogeneous value types (`DataType`).
/// * Per‑entry TTL / expiration management.
/// * An approximate LRU based eviction policy backed by an in‑memory `LruCache` of key hashes.
/// * Pub/Sub style list modification notifications used for blocking list commands (BLPOP / BRPOP).
/// * JSON (document) sub‑path manipulation helpers.
/// * Hash / List / String / Numeric primitives inspired by Redis semantics.
/// * Persistence (binary snapshot save / load) for durability between restarts.
///
/// Concurrency Model:
/// * Key/value storage uses `DashMap` for lock sharded concurrency.
/// * The LRU structure is protected by an `RwLock` but only accessed opportunistically using `try_write` / `try_read` to avoid contention on the hot path.
/// * All mutation helpers attempt to touch the LRU to maintain recency ordering; if the lock is contended, the miss is ignored (best effort policy).
///
/// Eviction:
/// * When capacity is exceeded, we collect a bounded set of recent keys (up to MAX_CANDIDATES) from the LRU and rank them via an eviction score provided by `CacheEntry`.
/// * We evict until the shard occupancy falls below 80% of configured capacity (hysteresis to reduce churn).
///
/// Safety & Invariants:
/// * `size` tracks logical key count (non expired). Expired keys are lazily removed during command access.
/// * Methods returning `DataType::Nil` follow Redis‑like semantics for absence or special conditions.
///
/// NOTE: Many methods are `async` to integrate uniformly with higher‑level async command handling even if they do not perform awaited IO themselves.
pub struct CalodShard {
    /// Main data storage
    data: DashMap<String, CacheEntry, ahash::RandomState>,
    /// LRU tracking - Using LRU to avoid new allocations
    lru_cache: Arc<RwLock<LruCache<u64, ()>>>,
    /// Shard capacity
    capacity: AtomicUsize,
    /// Current size
    size: AtomicUsize,
    /// Metrics
    metrics: Arc<CalodMetrics>,
    /// List modification notifier for PubSub
    list_modification_notifier: broadcast::Sender<String>,
    /// Shard ID
    id: usize,
}

impl CalodShard {
    /// Create a new shard with a maximum key capacity.
    ///
    /// * `capacity` – Logical maximum number of keys before eviction is triggered.
    /// * `metrics` – Shared metrics collector.
    /// * `id` – Monotonically assigned shard identifier.
    pub fn new(capacity: usize, metrics: Arc<CalodMetrics>, id: usize) -> Self {
        let data = DashMap::with_hasher(ahash::RandomState::new());
        // Unbounded recency tracker: it only orders keys for approximate-LRU
        // eviction, which is actually driven by the shard's `size`/`capacity`
        // atomics in `evict()`. Using `unbounded()` avoids preallocating a
        // `capacity`-sized map per shard (critical with multiple databases).
        let lru_cache = Arc::new(RwLock::new(LruCache::unbounded()));

        Self {
            data,
            lru_cache,
            capacity: AtomicUsize::new(capacity),
            size: AtomicUsize::new(0),
            list_modification_notifier: broadcast::channel(1024).0,
            metrics,
            id,
        }
    }

    /// Return this shard's numeric identifier.
    pub fn id(&self) -> usize {
        self.id
    }

    pub fn current_size(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }

    pub async fn clear_all(&self) {
        self.data.clear();
        self.size.store(0, Ordering::Relaxed);
        self.metrics.total_data_size.store(0, Ordering::Relaxed);
        if let Ok(mut lru) = self.lru_cache.write() {
            lru.clear();
        }
    }

    /// Command: TYPE – Return the stored value logical type for `key`.
    /// Returns `DataType::Nil` if key is expired or absent. For absent key we return a string "none" to match Redis semantics.
    pub async fn type_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        if let Some(entry_ref) = self.data.get(key) {
            let entry = entry_ref.value();
            if entry.is_expired() {
                drop(entry_ref);
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
                return Ok(DataType::Nil);
            }
            self.touch_key(key).await;
            let type_name = match &entry.value {
                DataType::String(_) | DataType::Status(_) => "string",
                DataType::List(_) => "list",
                DataType::Set(_) => "set",
                DataType::Hash(_) => "hash",
                DataType::Integer(_) => "integer",
                DataType::Document(_) => "document",
                DataType::Array(_) => "array",
                DataType::SortedSet(_) => "zset",
                DataType::Stream(_) => "stream",
                DataType::Double(_) => "string",
                DataType::Object { .. } => "object",
                DataType::Nil | DataType::NilArray => NONE_TYPE,
            };
            Ok(DataType::string_from(type_name))
        } else {
            Ok(DataType::string_from(NONE_TYPE))
        }
    }

    /// Command: KEYS – Return keys matching Redis-style glob pattern.
    pub async fn keys(&self, pattern: &str) -> Vec<String> {
        trace!("Shard getting keys with pattern: {}", pattern);

        let estimated_size = (self.size.load(Ordering::Relaxed) / 10).max(16);
        let mut results = Vec::with_capacity(estimated_size);

        self.data.iter().for_each(|entry| {
            if glob_matches(pattern, entry.key()) && !entry.value().is_expired() {
                results.push(entry.key().clone());
            }
        });

        results
    }

    /// Command: EXISTS – Return true if key exists and is not expired. Expired keys are purged lazily.
    #[inline]
    pub async fn exists(&self, key: &str) -> bool {
        trace!("Shard checking exists for key: {}", key);

        if let Some(entry_ref) = self.data.get(key) {
            let result = !entry_ref.is_expired();
            if !result {
                drop(entry_ref);
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
            } else {
                self.touch_key(key).await;
            }
            result
        } else {
            false
        }
    }

    /// Command: EXPIRE – Set a relative expiration (in seconds) for key.
    #[inline]
    pub async fn expire(&self, key: &str, seconds: u64) -> Result<bool, CacheError> {
        if let Some(mut entry_writer) = self.data.get_mut(key) {
            entry_writer
                .value_mut()
                .expire_in(Duration::from_secs(seconds));
            self.touch_key(key).await;
            Ok(true)
        } else {
            Err(CacheError::KeyNotFound(key.to_string()))
        }
    }

    /// Command: PEXPIRE – Set a relative expiration (in milliseconds) for key.
    #[inline]
    pub async fn pexpire(&self, key: &str, milliseconds: u64) -> Result<bool, CacheError> {
        if let Some(mut entry_writer) = self.data.get_mut(key) {
            entry_writer
                .value_mut()
                .expire_in(Duration::from_millis(milliseconds));
            self.touch_key(key).await;
            Ok(true)
        } else {
            Err(CacheError::KeyNotFound(key.to_string()))
        }
    }

    /// Command: EXPIREAT – Set absolute expiration timestamp in seconds.
    #[inline]
    pub async fn expireat(&self, key: &str, when_secs: u64) -> Result<bool, CacheError> {
        if let Some(mut entry_writer) = self.data.get_mut(key) {
            entry_writer
                .value_mut()
                .expire_at_ms(when_secs.saturating_mul(1000));
            self.touch_key(key).await;
            Ok(true)
        } else {
            Err(CacheError::KeyNotFound(key.to_string()))
        }
    }

    /// Command: PEXPIREAT – Set absolute expiration timestamp in milliseconds.
    #[inline]
    pub async fn pexpireat(&self, key: &str, when_ms: u64) -> Result<bool, CacheError> {
        if let Some(mut entry_writer) = self.data.get_mut(key) {
            entry_writer.value_mut().expire_at_ms(when_ms);
            self.touch_key(key).await;
            Ok(true)
        } else {
            Err(CacheError::KeyNotFound(key.to_string()))
        }
    }

    /// Command: TTL – Return remaining TTL in seconds (if any). Returns error if key is missing or expired.
    #[inline]
    pub async fn ttl(&self, key: &str) -> Result<Option<i64>, CacheError> {
        if let Some(entry_ref) = self.data.get(key) {
            let entry = entry_ref.value();
            if entry.is_expired() {
                drop(entry_ref); // Drop read guard before deletion
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
                return Err(CacheError::KeyExpired(key.to_string()));
            }

            self.touch_key(key).await;
            Ok(entry.ttl())
        } else {
            Err(CacheError::KeyNotFound(key.to_string()))
        }
    }

    /// Command: PERSIST – Remove the expiration from a key.
    #[inline]
    pub async fn persist(&self, key: &str) -> Result<bool, CacheError> {
        if let Some(mut entry_writer) = self.data.get_mut(key) {
            entry_writer.value_mut().persist();
            self.touch_key(key).await;
            Ok(true)
        } else {
            Err(CacheError::KeyNotFound(key.to_string()))
        }
    }

    /// Command: DEL – Delete a key. Returns true if key existed.
    #[inline]
    pub async fn delete(&self, key: &str) -> bool {
        trace!("Shard deleting key: {}", key);

        let result = if let Some((_, entry)) = self.data.remove(key) {
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.metrics
                .total_data_size
                .fetch_sub(entry.size() as u64, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            true
        } else {
            false
        };

        result
    }

    #[inline]
    fn stable_hash_key(key: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    pub async fn random_key(&self) -> Option<String> {
        self.data.iter().find_map(|entry| {
            if entry.value().is_expired() {
                None
            } else {
                Some(entry.key().clone())
            }
        })
    }

    /// Non-destructively clone a live entry (skipping/expiring stale ones).
    /// Used by cross-database `COPY`.
    pub async fn get_entry(&self, key: &str) -> Option<CacheEntry> {
        let entry_ref = self.data.get(key)?;
        if entry_ref.is_expired() {
            drop(entry_ref);
            self.data.remove(key);
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            return None;
        }
        Some(entry_ref.value().clone())
    }

    pub async fn take_entry(&self, key: &str) -> Option<CacheEntry> {
        let (_, entry) = self.data.remove(key)?;
        self.size.fetch_sub(1, Ordering::Relaxed);
        self.metrics
            .total_data_size
            .fetch_sub(entry.size() as u64, Ordering::Relaxed);
        self.remove_from_lru(key).await;
        Some(entry)
    }

    pub async fn put_entry(&self, key: String, entry: CacheEntry) -> Option<CacheEntry> {
        let exists = self.data.contains_key(&key);
        if !exists && self.size.load(Ordering::Relaxed) >= self.capacity.load(Ordering::Relaxed) {
            self.evict().await;
        }

        let entry_size = entry.size();
        let old = self.data.insert(key.clone(), entry);
        match &old {
            Some(prev) => {
                self.metrics
                    .total_data_size
                    .fetch_sub(prev.size() as u64, Ordering::Relaxed);
            }
            None => {
                self.size.fetch_add(1, Ordering::Relaxed);
            }
        }

        self.metrics
            .total_data_size
            .fetch_add(entry_size as u64, Ordering::Relaxed);
        self.touch_key(&key).await;
        old
    }

    pub async fn touch_cmd(&self, key: &str) -> bool {
        if let Some(entry_ref) = self.data.get(key) {
            if entry_ref.is_expired() {
                drop(entry_ref);
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
                return false;
            }
            self.touch_key(key).await;
            true
        } else {
            false
        }
    }

    /// Command: EXPIRETIME – Return absolute expiration timestamp in seconds.
    #[inline]
    pub async fn expiretime(&self, key: &str) -> Result<Option<i64>, CacheError> {
        if let Some(entry_ref) = self.data.get(key) {
            let entry = entry_ref.value();
            if entry.is_expired() {
                drop(entry_ref);
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
                return Err(CacheError::KeyExpired(key.to_string()));
            }

            self.touch_key(key).await;
            Ok(entry.expiretime())
        } else {
            Err(CacheError::KeyNotFound(key.to_string()))
        }
    }

    /// Command: PEXPIRETIME – Return absolute expiration timestamp in milliseconds.
    #[inline]
    pub async fn pexpiretime(&self, key: &str) -> Result<Option<i64>, CacheError> {
        if let Some(entry_ref) = self.data.get(key) {
            let entry = entry_ref.value();
            if entry.is_expired() {
                drop(entry_ref);
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
                return Err(CacheError::KeyExpired(key.to_string()));
            }

            self.touch_key(key).await;
            Ok(entry.pexpiretime())
        } else {
            Err(CacheError::KeyNotFound(key.to_string()))
        }
    }

    pub async fn rename_key(&self, key: &str, new_key: &str, nx: bool) -> Result<bool, CacheError> {
        if key == new_key {
            return Err(CacheError::InvalidCommandArguments(
                "ERR source and destination objects are the same".to_string(),
            ));
        }

        if nx && self.exists(new_key).await {
            return Ok(false);
        }

        let Some(moved_entry) = self.take_entry(key).await else {
            return Err(CacheError::KeyNotFound(key.to_string()));
        };

        let _ = self.put_entry(new_key.to_string(), moved_entry).await;

        Ok(true)
    }

    // Strings/Numbers Commands
    /// Command: GET – Fetch a key's value. Returns `KeyNotFound` or `KeyExpired` errors.
    #[inline]
    pub async fn get(&self, key: &str) -> Result<DataType, CacheError> {
        trace!("Shard getting key: {}", key);
        if let Some(entry_ref) = self.data.get(key) {
            let entry = entry_ref.value();
            if entry.is_expired() {
                drop(entry_ref);
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
                return Err(CacheError::KeyExpired(key.to_string()));
            }

            let result = entry.value.clone();
            self.touch_key(key).await;
            Ok(result)
        } else {
            Err(CacheError::KeyNotFound(key.to_string()))
        }
    }

    /// Command: SET – Set value with optional expiration + conditional flags (NX / XX).
    #[inline]
    pub async fn set_cmd(
        &self,
        key: String,
        value: DataType,
        expire_option: Option<SetExpireOption>,
        set_option: Option<SetOption>,
    ) -> Result<bool, CacheError> {
        let ttl_duration: Option<Duration> = match expire_option {
            Some(SetExpireOption::EX(seconds)) => Some(Duration::from_secs(seconds)),
            Some(SetExpireOption::PX(milliseconds)) => Some(Duration::from_millis(milliseconds)),
            None => None,
        };

        // Check NX/XX conditions first to avoid unnecessary work
        if let Some(option) = set_option {
            match option {
                SetOption::NX => {
                    if self.exists(&key).await {
                        return Ok(false);
                    }
                }
                SetOption::XX => {
                    if !self.exists(&key).await {
                        return Ok(false);
                    }
                }
            }
        }

        self.set(key, value, ttl_duration).await;
        Ok(true)
    }

    /// Internal helper implementing unconditional set with optional TTL.
    #[inline]
    pub async fn set(
        &self,
        key: String,
        value: DataType,
        ttl: Option<Duration>,
    ) -> Option<DataType> {
        trace!("Shard setting key: {}", key);

        // check if we need to evict before adding a new entry
        if self.size.load(Ordering::Relaxed) >= self.capacity.load(Ordering::Relaxed) {
            self.evict().await;
        }

        let entry = CacheEntry::new(value, ttl);
        let entry_size = entry.size();
        let old_entry = if let Some(old_e) = self.data.insert(key.clone(), entry) {
            self.metrics
                .total_data_size
                .fetch_sub(old_e.size() as u64, Ordering::Relaxed);
            Some(old_e.value)
        } else {
            self.size.fetch_add(1, Ordering::Relaxed);
            None
        };

        // update size metrics
        self.metrics
            .total_data_size
            .fetch_add(entry_size as u64, Ordering::Relaxed);
        self.touch_key(&key).await;
        old_entry
    }

    /// Command: APPEND – Append bytes to existing string or create a new one. Returns new length.
    #[inline]
    pub async fn append_cmd(&self, key: &str, value: Vec<u8>) -> Result<DataType, CacheError> {
        // Fast path for common case - check if key exists and is string
        if let Some(mut entry_writer) = self.data.get_mut(key) {
            let entry = entry_writer.value_mut();

            // Handle string case
            if let DataType::String(s) = &mut entry.value {
                s.extend_from_slice(&value);
                let new_len = s.len();
                self.touch_key(key).await;
                return Ok(DataType::Integer(new_len as i64));
            } else {
                return Err(CacheError::DataTypeMismatch(
                    key.to_string(),
                    "String".to_string(),
                    entry.value.data_type(),
                ));
            }
        }

        // Key doesn't exist, create new entry with pre-allocated capacity
        let len = value.len();
        self.set(key.to_string(), DataType::String(value), None)
            .await;
        Ok(DataType::Integer(len as i64))
    }

    /// Command: STRLEN – Length of string value or 0 for non existent / expired.
    #[inline]
    pub async fn strlen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        if let Some(entry_ref) = self.data.get(key) {
            let entry = entry_ref.value();

            if entry.is_expired() {
                drop(entry_ref);
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
                return Ok(DataType::Integer(0));
            }

            match &entry.value {
                DataType::String(s) => {
                    self.touch_key(key).await;
                    Ok(DataType::Integer(s.len() as i64))
                }
                DataType::Nil => Ok(DataType::Integer(0)),
                _ => Err(CacheError::DataTypeMismatch(
                    key.to_string(),
                    "string".to_string(),
                    entry.value.data_type(),
                )),
            }
        } else {
            Ok(DataType::Integer(0))
        }
    }

    /// Command: GETRANGE – Return substring (inclusive end index logic consistent with Redis).
    pub async fn getrange_cmd(
        &self,
        key: &str,
        start: isize,
        end: isize,
    ) -> Result<DataType, CacheError> {
        if let Some(entry_ref) = self.data.get(key) {
            let entry = entry_ref.value();

            if entry.is_expired() {
                drop(entry_ref);
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
                return Ok(DataType::String(Vec::new()));
            }

            if let DataType::String(s) = &entry.value {
                let len = s.len() as isize;

                let start_index = if start < 0 {
                    (start + len).max(0)
                } else {
                    start.min(len)
                } as usize;
                let end_index = if end < 0 {
                    (end + len).max(0)
                } else {
                    end.min(len)
                } as usize;

                if start_index > end_index || start_index >= s.len() {
                    return Ok(DataType::String(Vec::new()));
                }

                // Byte-wise slicing (binary-safe). End index is inclusive per Redis.
                let result = if end_index >= s.len() {
                    s[start_index..].to_vec()
                } else {
                    s[start_index..=end_index].to_vec()
                };

                self.touch_key(key).await;
                Ok(DataType::String(result))
            } else {
                Err(CacheError::DataTypeMismatch(
                    key.to_string(),
                    "string".to_string(),
                    entry.value.data_type(),
                ))
            }
        } else {
            Ok(DataType::String(Vec::new()))
        }
    }

    /// Command: SETRANGE – Overwrite string starting at `offset`, padding with null bytes if required.
    #[inline]
    pub async fn setrange_cmd(
        &self,
        key: &str,
        offset: usize,
        value: Vec<u8>,
    ) -> Result<DataType, CacheError> {
        if value.is_empty() {
            // Fast path for empty value - just return current length
            if let Some(entry_ref) = self.data.get(key) {
                if let DataType::String(s) = &entry_ref.value().value {
                    let len = s.len().max(offset) as i64;
                    return Ok(DataType::Integer(len));
                }
            }
        }

        // Get or create entry
        let mut entry_writer = match self.data.get_mut(key) {
            Some(writer) => writer,
            None => {
                // Create new empty buffer if key does not exist, zero-padded to offset.
                let new_string = if offset > 0 {
                    let mut s = Vec::with_capacity(offset + value.len());
                    s.resize(offset, 0u8);
                    s
                } else {
                    Vec::new()
                };

                self.set(key.to_string(), DataType::String(new_string), None)
                    .await;
                self.data.get_mut(key).unwrap() // Safe since we just inserted it
            }
        };

        let entry = entry_writer.value_mut();

        if let DataType::String(s) = &mut entry.value {
            // Ensure the buffer is long enough, padding with zero bytes.
            if offset > s.len() {
                s.resize(offset, 0u8);
            }

            // Byte-wise overwrite starting at offset (binary-safe).
            let end = offset + value.len();
            if end > s.len() {
                s.resize(end, 0u8);
            }
            s[offset..end].copy_from_slice(&value);

            self.touch_key(key).await;
            Ok(DataType::Integer(s.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "String".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: GETSET – Atomically get previous value and set to new string.
    #[inline]
    pub async fn getset_cmd(&self, key: &str, value: Vec<u8>) -> Result<DataType, CacheError> {
        let result = if let Some(entry_ref) = self.data.get(key) {
            if entry_ref.is_expired() {
                drop(entry_ref);
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
                DataType::Nil
            } else {
                entry_ref.value().value.clone()
            }
        } else {
            DataType::Nil
        };

        self.set(key.to_string(), DataType::String(value), None)
            .await;
        Ok(result)
    }

    /// Command: INCR – Increment integer value (auto initialize as 0). Errors if not integer-parsable.
    pub async fn incr_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        self.incrby_cmd(key, 1).await
    }

    /// Command: DECR – Decrement integer value.
    pub async fn decr_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        self.incrby_cmd(key, -1).await
    }

    /// Command: INCRBY – Adjust integer by `increment`.
    #[inline]
    pub async fn incrby_cmd(&self, key: &str, increment: i64) -> Result<DataType, CacheError> {
        if let Some(mut entry_writer) = self.data.get_mut(key) {
            let entry = entry_writer.value_mut();

            match &mut entry.value {
                DataType::Integer(n) => {
                    *n += increment;
                    self.touch_key(key).await;
                    return Ok(DataType::Integer(*n));
                }
                DataType::String(s) => match std::str::from_utf8(s)
                    .ok()
                    .and_then(|t| t.trim().parse::<i64>().ok())
                {
                    Some(mut n) => {
                        n += increment;
                        entry.value = DataType::Integer(n);
                        self.touch_key(key).await;
                        return Ok(DataType::Integer(n));
                    }
                    None => return Err(CacheError::NotAnInteger),
                },
                DataType::Nil => {
                    // Treat Nil as 0 for INCRBY
                    entry.value = DataType::Integer(increment);
                    self.touch_key(key).await;
                    return Ok(DataType::Integer(increment));
                }
                _ => {
                    return Err(CacheError::DataTypeMismatch(
                        key.to_string(),
                        "Integer or String representable as integer".to_string(),
                        entry.value.data_type(),
                    ))
                }
            }
        }

        self.set(key.to_string(), DataType::Integer(increment), None)
            .await;
        Ok(DataType::Integer(increment))
    }

    /// Command: DECRBY – Adjust integer by negative decrement.
    pub async fn decrby_cmd(&self, key: &str, decrement: i64) -> Result<DataType, CacheError> {
        // Reuse incrby_cmd with negative increment for DECRBY
        self.incrby_cmd(key, -decrement).await
    }

    /// Command: INCRBYFLOAT – Increment floating-point stored as string.
    pub async fn incrbyfloat_cmd(&self, key: &str, increment: f64) -> Result<DataType, CacheError> {
        if let Some(mut entry_writer) = self.data.get_mut(key) {
            let entry = entry_writer.value_mut();

            let result = match &mut entry.value {
                DataType::String(s) => match std::str::from_utf8(s)
                    .ok()
                    .and_then(|t| t.trim().parse::<f64>().ok())
                {
                    Some(n) => n + increment,
                    None => return Err(CacheError::InvalidScoreFormat),
                },
                DataType::Nil => increment,
                _ => {
                    return Err(CacheError::DataTypeMismatch(
                        key.to_string(),
                        "String representable as float".to_string(),
                        entry.value.data_type(),
                    ))
                }
            };

            let result_str = format!("{}", result);
            entry.value = DataType::string_from(result_str.clone());
            self.touch_key(key).await;

            Ok(DataType::string_from(result_str))
        } else {
            let result_str = format!("{}", increment);
            self.set(
                key.to_string(),
                DataType::string_from(result_str.clone()),
                None,
            )
            .await;
            Ok(DataType::string_from(result_str))
        }
    }

    // Hash Commands
    /// Command: HSET – Set one or more hash fields. Returns number of new fields added.
    pub async fn hset_cmd(
        &self,
        key: &str,
        field_values: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<DataType, CacheError> {
        let mut entry_mut = self
            .data
            .entry(key.to_string())
            .or_insert_with(|| CacheEntry::new(DataType::Hash(DashMap::new()), None));
        let entry = entry_mut.value_mut();
        let mut count_new_fields = 0;

        if let DataType::Hash(hash_map) = &mut entry.value {
            for (field, value) in field_values {
                if !hash_map.contains_key(&field) {
                    count_new_fields += 1;
                }
                hash_map.insert(field, value);
            }
            self.touch_key(key).await;
            Ok(DataType::Integer(count_new_fields)) // Return number of new fields added
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: HGET – Get a single hash field.
    pub async fn hget_cmd(&self, key: &str, field: Vec<u8>) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Nil);
        };
        let entry = entry_ref.value();

        if let DataType::Hash(hash_map) = &entry.value {
            match hash_map.get(field.as_slice()) {
                Some(value) => {
                    self.touch_key(key).await;
                    Ok(DataType::string_from(value.clone()))
                }
                None => Ok(DataType::Nil), // Field not found returns Nil Bulk String
            }
        } else if matches!(entry.value, DataType::Nil) {
            Ok(DataType::Nil)
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: HDEL – Delete one or more hash fields. Returns count removed.
    pub async fn hdel_cmd(&self, key: &str, fields: Vec<Vec<u8>>) -> Result<DataType, CacheError> {
        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_writer.value_mut();
        let mut count_deleted_fields = 0;

        if let DataType::Hash(hash_map) = &mut entry.value {
            for field in fields {
                if hash_map.remove(field.as_slice()).is_some() {
                    count_deleted_fields += 1;
                }
            }
            self.touch_key(key).await;
            Ok(DataType::Integer(count_deleted_fields)) // Return number of deleted fields
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: HEXISTS – Test hash field existence (1 / 0).
    pub async fn hexists_cmd(&self, key: &str, field: Vec<u8>) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_ref.value();

        if let DataType::Hash(hash_map) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Integer(
                if hash_map.contains_key(field.as_slice()) {
                    1
                } else {
                    0
                },
            )) // 1 if field exists, 0 if not
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: HGETALL – Return full hash snapshot.
    pub async fn hgetall_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::List(Vec::new()));
        };
        let entry = entry_ref.value();

        if let DataType::Hash(hash_map) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Hash(hash_map.clone())) // Return a clone to avoid borrowing issues
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: HINCRBY – Increment integer stored at hash field.
    pub async fn hincrby_cmd(
        &self,
        key: &str,
        field: Vec<u8>,
        increment: i64,
    ) -> Result<DataType, CacheError> {
        let mut entry_mut = self
            .data
            .entry(key.to_string())
            .or_insert_with(|| CacheEntry::new(DataType::Hash(DashMap::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::Hash(hash_map) = &mut entry.value {
            let mut current_value: i64 = 0;
            if let Some(val_str) = hash_map.get(field.as_slice()) {
                current_value = std::str::from_utf8(&val_str)
                    .ok()
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .ok_or(CacheError::NotAnInteger)?;
            }
            current_value += increment;
            hash_map.insert(field, current_value.to_string().into_bytes());
            self.touch_key(key).await;
            Ok(DataType::Integer(current_value))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: HINCRBYFLOAT – Increment float stored at hash fields (batched variant returning updated map subset).
    pub async fn hincrbyfloat_cmd(
        &self,
        key: &str,
        key_field_increment: Vec<(Vec<u8>, f64)>,
    ) -> Result<DataType, CacheError> {
        let mut entry_mut = self
            .data
            .entry(key.to_string())
            .or_insert_with(|| CacheEntry::new(DataType::Hash(DashMap::new()), None));
        let entry = entry_mut.value_mut();
        let response_map = DashMap::new();

        if let DataType::Hash(hash_map) = &mut entry.value {
            for (field, increment) in key_field_increment {
                let mut current_value: f64 = 0.0;
                if let Some(val_str) = hash_map.get(field.as_slice()) {
                    current_value = std::str::from_utf8(&val_str)
                        .ok()
                        .and_then(|s| s.trim().parse::<f64>().ok())
                        .ok_or(CacheError::InvalidScoreFormat)?;
                }
                current_value += increment;
                hash_map.insert(field.clone(), current_value.to_string().into_bytes());
                response_map.insert(field, current_value.to_string().into_bytes());
                // Store for response
            }
            self.touch_key(key).await;
            Ok(DataType::Hash(response_map)) // Returning a Hash DataType with all updated fields and values
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: HKEYS – Return vector of field names.
    pub async fn hkeys_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::List(Vec::new()));
        };
        let entry = entry_ref.value();

        if let DataType::Hash(hash_map) = &entry.value {
            self.touch_key(key).await;
            let keys: Vec<Vec<u8>> = hash_map.iter().map(|entry| entry.key().clone()).collect();
            Ok(DataType::List(keys))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: HLEN – Return number of fields.
    pub async fn hlen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_ref.value();

        if let DataType::Hash(hash_map) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Integer(hash_map.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: HMGET – Bulk get of multiple hash fields.
    pub async fn hmget_cmd(&self, key: &str, fields: Vec<Vec<u8>>) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Array(vec![DataType::Nil; fields.len()]));
        };
        let entry = entry_ref.value();
        let mut results: Vec<DataType> = Vec::with_capacity(fields.len());

        if let DataType::Hash(hash_map) = &entry.value {
            for field in fields {
                match hash_map.get(field.as_slice()) {
                    Some(value) => results.push(DataType::string_from(value.clone())),
                    None => results.push(DataType::Nil),
                }
            }
            self.touch_key(key).await;
            Ok(DataType::Array(results))
        } else if matches!(entry.value, DataType::Nil) {
            Ok(DataType::Array(vec![DataType::Nil; fields.len()]))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: PTTL – Return remaining TTL in milliseconds.
    #[inline]
    pub async fn pttl(&self, key: &str) -> Result<Option<i64>, CacheError> {
        if let Some(entry_ref) = self.data.get(key) {
            let entry = entry_ref.value();
            if entry.is_expired() {
                drop(entry_ref);
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
                return Err(CacheError::KeyExpired(key.to_string()));
            }

            self.touch_key(key).await;
            Ok(entry.pttl())
        } else {
            Err(CacheError::KeyNotFound(key.to_string()))
        }
    }

    /// Command: HMSET – Bulk set of multiple hash fields.
    pub async fn hmset_cmd(
        &self,
        key: &str,
        field_values: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<DataType, CacheError> {
        let mut entry_mut = self
            .data
            .entry(key.to_string())
            .or_insert_with(|| CacheEntry::new(DataType::Hash(DashMap::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::Hash(hash_map) = &mut entry.value {
            for (field, value) in field_values {
                hash_map.insert(field, value);
            }
            self.touch_key(key).await;
            Ok(DataType::Status(OK_RESPONSE.to_string())) // HMSET returns OK Simple String
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: HSETNX – Set hash field only if absent.
    pub async fn hsetnx_cmd(
        &self,
        key: &str,
        field: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<DataType, CacheError> {
        let mut entry_mut = self
            .data
            .entry(key.to_string())
            .or_insert_with(|| CacheEntry::new(DataType::Hash(DashMap::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::Hash(hash_map) = &mut entry.value {
            if !hash_map.contains_key(field.as_slice()) {
                hash_map.insert(field, value);
                self.touch_key(key).await;
                Ok(DataType::Integer(1)) // 1 if field was set
            } else {
                Ok(DataType::Integer(0)) // 0 if field was not set (already exists)
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: HVALS – Return collection of hash values.
    pub async fn hvals_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::List(Vec::new()));
        };
        let entry = entry_ref.value();

        if let DataType::Hash(hash_map) = &entry.value {
            self.touch_key(key).await;
            let values: Vec<Vec<u8>> = hash_map.iter().map(|entry| entry.value().clone()).collect();
            Ok(DataType::List(values))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Hash".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    // List commands

    /// Command: LPUSH – Push values to the head of list (reverse order of arguments).
    pub async fn lpush_cmd(
        &self,
        key: String,
        values: Vec<Vec<u8>>,
    ) -> Result<DataType, CacheError> {
        let mut entry_mut = self
            .data
            .entry(key.clone())
            .or_insert_with(|| CacheEntry::new(DataType::List(Vec::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::List(list) = &mut entry.value {
            for value in values {
                list.insert(0, value);
            }
            self.touch_key(&key).await;
            let _ = self.list_modification_notifier.send(key.clone());
            Ok(DataType::Integer(list.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key,
                "List".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: RPUSH – Push values to the tail of list.
    pub async fn rpush_cmd(
        &self,
        key: String,
        values: Vec<Vec<u8>>,
    ) -> Result<DataType, CacheError> {
        let mut entry_mut = self
            .data
            .entry(key.clone())
            .or_insert_with(|| CacheEntry::new(DataType::List(Vec::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::List(list) = &mut entry.value {
            for value in values {
                list.push(value);
            }
            self.touch_key(&key).await;
            let _ = self.list_modification_notifier.send(key.clone());
            Ok(DataType::Integer(list.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key,
                "List".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: LPOP – Pop head element.
    pub async fn lpop_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Ok(DataType::Nil);
        };
        let entry = entry_writer.value_mut();

        if let DataType::List(list) = &mut entry.value {
            if list.is_empty() {
                Ok(DataType::Nil)
            } else {
                let value = list.remove(0);
                self.touch_key(key).await;
                Ok(DataType::string_from(value))
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "List".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: RPOP – Pop tail element.
    pub async fn rpop_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Ok(DataType::Nil);
        };
        let entry = entry_writer.value_mut();

        if let DataType::List(list) = &mut entry.value {
            if list.is_empty() {
                Ok(DataType::Nil)
            } else {
                let value = list.pop().unwrap(); // Safe because we checked for emptiness
                self.touch_key(key).await;
                Ok(DataType::string_from(value))
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "List".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: LLEN – List length.
    pub async fn llen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_ref.value();

        if let DataType::List(list) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Integer(list.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "List".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: LRANGE – Return sub-slice (inclusive end) of list.
    pub async fn lrange_cmd(
        &self,
        key: &str,
        start: isize,
        end: isize,
    ) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::List(Vec::new()));
        };
        let entry = entry_ref.value();

        if let DataType::List(list) = &entry.value {
            let len = list.len() as isize;
            let start_index = if start < 0 {
                (start + len).max(0)
            } else {
                start
            }
            .min(len);
            let end_index = if end < 0 { (end + len).max(0) } else { end }.min(len);

            if start_index > end_index {
                return Ok(DataType::List(Vec::new())); // Empty range
            }

            let range = list[start_index as usize..end_index as usize + 1].to_vec();
            self.touch_key(key).await;
            Ok(DataType::List(range)) // Convert Vec<String> to LinkedList<String>
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "List".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: LINDEX – Random access by positive / negative index.
    pub async fn lindex_cmd(&self, key: &str, index: isize) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Nil);
        };
        let entry = entry_ref.value();

        if let DataType::List(list) = &entry.value {
            let len = list.len() as isize;
            let index_pos = if index < 0 { index + len } else { index };
            if index_pos < 0 || index_pos >= len {
                return Ok(DataType::Nil); // Index out of range returns Nil Bulk String
            }
            self.touch_key(key).await;
            Ok(DataType::string_from(list[index_pos as usize].clone()))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "List".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: LINSERT – Insert before / after pivot.
    pub async fn linsert_cmd(
        &self,
        key: &str,
        before_after: InsertOption,
        pivot: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<DataType, CacheError> {
        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_writer.value_mut();

        if let DataType::List(list) = &mut entry.value {
            if let Some(pivot_index) = list.iter().position(|x| x == &pivot) {
                let insert_index = match before_after {
                    InsertOption::Before => pivot_index,
                    InsertOption::After => pivot_index + 1,
                };
                list.insert(insert_index, value);
                self.touch_key(key).await;
                Ok(DataType::Integer(list.len() as i64))
            } else {
                Ok(DataType::Integer(-1)) // Pivot not found returns -1
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "List".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: LSET – Overwrite element at index.
    pub async fn lset_cmd(
        &self,
        key: &str,
        index: isize,
        value: Vec<u8>,
    ) -> Result<DataType, CacheError> {
        let mut entry_writer = self
            .data
            .get_mut(key)
            .ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::List(list) = &mut entry.value {
            let len = list.len() as isize;
            let index_pos = if index < 0 { index + len } else { index };

            if index_pos < 0 || index_pos >= len {
                return Err(CacheError::IndexOutOfRange); // Index out of range returns error
            }
            list[index_pos as usize] = value;
            self.touch_key(key).await;
            Ok(DataType::Status(OK_RESPONSE.to_string())) // LSET returns OK Simple String
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "List".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: LTRIM – Keep only specified inclusive range.
    pub async fn ltrim_cmd(
        &self,
        key: &str,
        start: isize,
        end: isize,
    ) -> Result<DataType, CacheError> {
        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Ok(DataType::Status(OK_RESPONSE.to_string()));
        };
        let entry = entry_writer.value_mut();

        if let DataType::List(list) = &mut entry.value {
            let len = list.len() as isize;
            let start_index = if start < 0 {
                (start + len).max(0)
            } else {
                start
            }
            .min(len);
            let end_index = if end < 0 { (end + len).max(0) } else { end }.min(len);

            if start_index > end_index {
                list.clear(); // Trim to empty list if range is invalid
            } else {
                let trimmed_list: Vec<Vec<u8>> =
                    list[start_index as usize..=end_index as usize].to_vec();
                entry.value = DataType::List(trimmed_list); // Replace list with trimmed version
            }
            self.touch_key(key).await;
            Ok(DataType::Status(OK_RESPONSE.to_string())) // LTRIM returns OK Simple String
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "List".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: LREM – Remove occurrences of value according to count semantics.
    pub async fn lrem_cmd(
        &self,
        key: &str,
        count: i64,
        value: Vec<u8>,
    ) -> Result<DataType, CacheError> {
        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_writer.value_mut();

        if let DataType::List(list) = &mut entry.value {
            let removed_count = if count > 0 {
                Self::lrem_positive_count(list, count as usize, &value)
            } else if count < 0 {
                Self::lrem_negative_count(list, (-count) as usize, &value)
            } else {
                Self::lrem_zero_count(list, &value)
            };

            if removed_count > 0 {
                self.touch_key(key).await;
            }
            Ok(DataType::Integer(removed_count as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "List".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    fn lrem_positive_count(list: &mut Vec<Vec<u8>>, count: usize, value: &[u8]) -> usize {
        let mut removed_count = 0;
        let mut i = 0;
        while i < list.len() && removed_count < count {
            if list[i] == value {
                list.remove(i);
                removed_count += 1;
            } else {
                i += 1;
            }
        }
        removed_count
    }

    fn lrem_negative_count(list: &mut Vec<Vec<u8>>, count: usize, value: &[u8]) -> usize {
        let mut removed_count = 0;
        let mut i = list.len() as isize - 1;
        while i >= 0 && removed_count < count {
            if list[i as usize] == value {
                list.remove(i as usize);
                removed_count += 1;
            }
            i -= 1;
        }
        removed_count
    }

    fn lrem_zero_count(list: &mut Vec<Vec<u8>>, value: &[u8]) -> usize {
        let initial_len = list.len();
        list.retain(|x| x != value);
        initial_len - list.len() // Count of removed elements
    }

    /// Command: RPOPLPUSH – Pop from source tail and push to destination head.
    pub async fn rpoplpush_cmd(
        &self,
        source: &str,
        destination: &str,
    ) -> Result<DataType, CacheError> {
        let popped_value_result = self.rpop_cmd(source).await; // RPOP from source list

        match popped_value_result {
            Ok(DataType::String(popped_value)) => {
                // List elements are binary-safe (raw bytes); re-push the bytes
                // verbatim and return the original bytes so the reply is
                // binary-safe.
                self.lpush_cmd(destination.to_string(), vec![popped_value.clone()])
                    .await?; // LPUSH to destination
                Ok(DataType::String(popped_value)) // Return popped value (binary-safe)
            }
            Ok(DataType::Nil) => Ok(DataType::Nil), // Source list empty, return Nil Bulk String
            Ok(_) => Err(CacheError::DataTypeMismatch(
                source.to_string(),
                "List".to_string(),
                "Non-List".to_string(),
            )), // Should not happen if rpop_cmd is correctly implemented for lists
            Err(e) => Err(e),                       // Propagate errors from rpop_cmd
        }
    }

    /// Command: BLPOP – Blocking pop (head) from first non-empty list among `keys` within timeout.
    pub async fn blpop_cmd(
        &self,
        keys: Vec<String>,
        timeout_sec: f64,
    ) -> Result<DataType, CacheError> {
        let timeout_duration = Duration::from_secs_f64(timeout_sec.max(0.0));
        let mut notification_receiver = self.list_modification_notifier.subscribe();

        // initial non-blocking call
        for key in &keys {
            if let Ok(DataType::String(value)) = self.lpop_cmd(key).await {
                if value != b"nil" {
                    return Ok(DataType::List(Vec::<Vec<u8>>::from([
                        key.clone().into_bytes(),
                        value,
                    ])));
                }
            }
        }

        let start_time = tokio::time::Instant::now();
        loop {
            let remaining_timeout = timeout_duration.saturating_sub(start_time.elapsed());
            if remaining_timeout.is_zero() {
                // Blocking-pop timeout: Redis replies with a null *array* (*-1),
                // not a null bulk string.
                return Ok(DataType::NilArray);
            }

            match timeout(remaining_timeout, notification_receiver.recv()).await {
                Ok(Ok(notified_key)) => {
                    if keys.contains(&notified_key) {
                        if let Ok(DataType::String(value)) = self.lpop_cmd(&notified_key).await {
                            if value != b"nil" {
                                return Ok(DataType::List(Vec::<Vec<u8>>::from([
                                    notified_key.into_bytes(),
                                    value,
                                ])));
                            }
                        }
                    }
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    error!("List modification notification channel closed unexpectedly.");
                    return Err(CacheError::InternalError(
                        "List notification channel closed".to_string(),
                    ));
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) | Err(_) => {
                    return Ok(DataType::NilArray);
                }
            }
        }
    }

    /// Command: BRPOP – Blocking pop (tail) variant.
    pub async fn brpop_cmd(
        &self,
        keys: Vec<String>,
        timeout_sec: f64,
    ) -> Result<DataType, CacheError> {
        let timeout_duration = Duration::from_secs_f64(timeout_sec.max(0.0));
        let mut notification_receiver = self.list_modification_notifier.subscribe();

        for key in &keys {
            if let Ok(DataType::String(value)) = self.rpop_cmd(key).await {
                if value != b"nil" {
                    return Ok(DataType::List(Vec::<Vec<u8>>::from([
                        key.clone().into_bytes(),
                        value,
                    ])));
                }
            }
        }

        // 2. Blocking wait with timeout
        let start_time = tokio::time::Instant::now();
        loop {
            let remaining_timeout = timeout_duration.saturating_sub(start_time.elapsed());
            if remaining_timeout.is_zero() {
                // Blocking-pop timeout: Redis replies with a null *array* (*-1),
                // not a null bulk string.
                return Ok(DataType::NilArray);
            }

            match timeout(remaining_timeout, notification_receiver.recv()).await {
                Ok(Ok(notified_key)) => {
                    if keys.contains(&notified_key) {
                        if let Ok(DataType::String(value)) = self.rpop_cmd(&notified_key).await {
                            if value != b"nil" {
                                return Ok(DataType::List(Vec::<Vec<u8>>::from([
                                    notified_key.into_bytes(),
                                    value,
                                ])));
                            }
                        }
                    }
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    error!("List modification notification channel closed unexpectedly.");
                    return Err(CacheError::InternalError(
                        "List notification channel closed".to_string(),
                    ));
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) | Err(_) => {
                    return Ok(DataType::NilArray);
                }
            }
        }
    }

    // --- JSON Command Handlers ---
    /// Command: JSON.SET – Set JSON value at JSONPath.
    pub async fn json_set_cmd(
        &self,
        key: String,
        path: String,
        value_str: String,
    ) -> Result<DataType, CacheError> {
        let mut entry_mut = self
            .data
            .entry(key.clone())
            .or_insert_with(|| CacheEntry::new(DataType::Document(JsonValue::Null), None));
        let entry = entry_mut.value_mut();

        let json_value: JsonValue =
            serde_json::from_str(&value_str).map_err(|e: SerdeJsonError| {
                CacheError::InvalidCommandArguments(format!("Invalid JSON value: {}", e))
            })?;

        if let DataType::Document(doc) = &mut entry.value {
            jsonpath_set(doc, &path, json_value)?;
            self.touch_key(&key).await;
            Ok(DataType::Status(OK_RESPONSE.to_string()))
        } else {
            Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: JSON.GET – Get JSON sub-value by path.
    pub async fn json_get_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let result_json_value = jsonpath_get(&doc, &path)?;
                Ok(DataType::Document(result_json_value))
            }
            DataType::Nil => Ok(DataType::Nil), // Key not found returns Nil
            other => Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                other.data_type(),
            )),
        }
    }

    /// Command: JSON.DEL – Delete JSON path returning number removed.
    pub async fn json_del_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self
            .data
            .get_mut(&key)
            .ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Document(doc) = &mut entry.value {
            let deleted_count = jsonpath_del(doc, &path)?;
            self.touch_key(&key).await;
            Ok(DataType::Integer(deleted_count as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: JSON.TYPE – Return type string for JSON path.
    pub async fn json_type_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let json_value = jsonpath_get(&doc, &path)?;
                Ok(DataType::string_from(json_type_to_string(&json_value)))
            }
            DataType::Nil => Ok(DataType::Nil), // Key not found returns Nil
            other => Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                other.data_type(),
            )),
        }
    }

    /// Command: JSON.NUMINCRBY – Numeric add at JSON path.
    pub async fn json_numincrby_cmd(
        &self,
        key: String,
        path: String,
        increment: f64,
    ) -> Result<DataType, CacheError> {
        let mut entry_writer = self
            .data
            .get_mut(&key)
            .ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Document(doc) = &mut entry.value {
            let updated_json_value = jsonpath_numincrby(doc, &path, increment)?;
            self.touch_key(&key).await;
            Ok(DataType::Document(updated_json_value))
        } else {
            Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: JSON.STRAPPEND – Append to JSON string.
    pub async fn json_strappend_cmd(
        &self,
        key: String,
        path: String,
        value: String,
    ) -> Result<DataType, CacheError> {
        let mut entry_writer = self
            .data
            .get_mut(&key)
            .ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Document(doc) = &mut entry.value {
            let updated_json_value = jsonpath_strappend(doc, &path, &value)?;
            self.touch_key(&key).await;
            Ok(DataType::Document(updated_json_value))
        } else {
            Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: JSON.ARRAPPEND – Append elements to JSON array path.
    pub async fn json_arrappend_cmd(
        &self,
        key: String,
        path: String,
        values: Vec<String>,
    ) -> Result<DataType, CacheError> {
        let mut entry_writer = self
            .data
            .get_mut(&key)
            .ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Document(doc) = &mut entry.value {
            let json_values: Result<Vec<JsonValue>, CacheError> = values
                .iter()
                .map(|v_str| {
                    serde_json::from_str(v_str).map_err(|e| {
                        CacheError::InvalidCommandArguments(format!(
                            "Invalid JSON value in ARRAPPEND: {}",
                            e
                        ))
                    })
                })
                .collect();
            let updated_json_value = jsonpath_arrappend(doc, &path, &json_values?)?;
            self.touch_key(&key).await;
            Ok(DataType::Document(updated_json_value))
        } else {
            Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: JSON.OBJSET – Set object field at path.
    pub async fn json_objset_cmd(
        &self,
        key: String,
        path: String,
        key_to_set: String,
        value: String,
    ) -> Result<DataType, CacheError> {
        let mut entry_writer = self
            .data
            .get_mut(&key)
            .ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();
        let value_json: JsonValue = serde_json::from_str(&value).map_err(|e| {
            CacheError::InvalidCommandArguments(format!("Invalid JSON value in OBJSET: {}", e))
        })?;

        if let DataType::Document(doc) = &mut entry.value {
            let updated_json_value = jsonpath_objset(doc, &path, &key_to_set, value_json)?;
            self.touch_key(&key).await;
            Ok(DataType::Document(updated_json_value))
        } else {
            Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: JSON.OBJKEYS – List keys at object path.
    pub async fn json_objkeys_cmd(
        &self,
        key: String,
        path: String,
    ) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let keys = jsonpath_objkeys(&doc, &path)?;
                Ok(DataType::List(
                    keys.into_iter().map(String::into_bytes).collect(),
                ))
            }
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                other.data_type(),
            )),
        }
    }

    /// Command: JSON.OBJLEN – Number of keys at object path.
    pub async fn json_objlen_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let len = jsonpath_objlen(&doc, &path)?;
                Ok(DataType::Integer(len as i64))
            }
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                other.data_type(),
            )),
        }
    }

    /// Command: JSON.ARRINDEX – Find index of value (optionally within range).
    pub async fn json_arrindex_cmd(
        &self,
        key: String,
        path: String,
        value: String,
        range: Option<(isize, isize)>,
    ) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let value_json: JsonValue = serde_json::from_str(&value).map_err(|e| {
                    CacheError::InvalidCommandArguments(format!(
                        "Invalid JSON value in ARRINDEX: {}",
                        e
                    ))
                })?;
                let index = jsonpath_arrindex(&doc, &path, &value_json, range)?;
                Ok(DataType::Integer(index as i64))
            }
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                other.data_type(),
            )),
        }
    }

    /// Command: JSON.ARRINSERT – Insert into JSON array at index.
    pub async fn json_arrinsert_cmd(
        &self,
        key: String,
        path: String,
        index: isize,
        values: Vec<String>,
    ) -> Result<DataType, CacheError> {
        let mut entry_writer = self
            .data
            .get_mut(&key)
            .ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();
        let json_values: Result<Vec<JsonValue>, CacheError> = values
            .iter()
            .map(|v_str| {
                serde_json::from_str(v_str).map_err(|e| {
                    CacheError::InvalidCommandArguments(format!(
                        "Invalid JSON value in ARRINSERT: {}",
                        e
                    ))
                })
            })
            .collect();

        if let DataType::Document(doc) = &mut entry.value {
            let updated_json_value = jsonpath_arrinsert(doc, &path, index, &json_values?)?;
            self.touch_key(&key).await;
            Ok(DataType::Document(updated_json_value))
        } else {
            Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: JSON.ARRLEN – Length of JSON array at path.
    pub async fn json_arrlen_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let len = jsonpath_arrlen(&doc, &path)?;
                Ok(DataType::Integer(len as i64))
            }
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                other.data_type(),
            )),
        }
    }

    /// Command: JSON.ARRPOP – Pop element from JSON array path (optional index).
    pub async fn json_arrpop_cmd(
        &self,
        key: String,
        path: String,
        index: Option<isize>,
    ) -> Result<DataType, CacheError> {
        let mut entry_writer = self
            .data
            .get_mut(&key)
            .ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Document(doc) = &mut entry.value {
            let popped_value = jsonpath_arrpop(doc, &path, index)?;
            self.touch_key(&key).await;
            Ok(DataType::Document(popped_value))
        } else {
            Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: JSON.ARRTRIM – Trim JSON array.
    pub async fn json_arrtrim_cmd(
        &self,
        key: String,
        path: String,
        start: isize,
        stop: isize,
    ) -> Result<DataType, CacheError> {
        let mut entry_writer = self
            .data
            .get_mut(&key)
            .ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Document(doc) = &mut entry.value {
            let trimmed_len = jsonpath_arrtrim(doc, &path, start, stop)?;
            self.touch_key(&key).await;
            Ok(DataType::Integer(trimmed_len as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key,
                "Document".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    #[inline]
    /// Mark key hash as recently used in LRU (best effort, non-blocking on contention).
    pub async fn touch_key_with_hash(&self, key_hash: u64) {
        if let Ok(mut lru) = self.lru_cache.try_write() {
            lru.put(key_hash, ());
        }
    }

    /// Touch a key in the LRU (mark as recently used)
    #[inline]
    pub async fn touch_key(&self, key: &str) {
        let hash = Self::stable_hash_key(key);
        self.touch_key_with_hash(hash).await;
    }

    // Set commands

    pub async fn sadd_cmd(&self, key: &str, members: Vec<Vec<u8>>) -> Result<DataType, CacheError> {
        let mut added = 0_i64;
        let mut entry_mut = self
            .data
            .entry(key.to_string())
            .or_insert_with(|| CacheEntry::new(DataType::Set(DashSet::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::Set(set) = &mut entry.value {
            for member in members {
                if set.insert(member) {
                    added += 1;
                }
            }
            self.touch_key(key).await;
            Ok(DataType::Integer(added))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Set".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    pub async fn srem_cmd(&self, key: &str, members: Vec<Vec<u8>>) -> Result<DataType, CacheError> {
        let Some(mut entry_mut) = self.data.get_mut(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_mut.value_mut();

        if let DataType::Set(set) = &mut entry.value {
            let mut removed = 0_i64;
            for member in members {
                if set.remove(member.as_slice()).is_some() {
                    removed += 1;
                }
            }
            self.touch_key(key).await;
            Ok(DataType::Integer(removed))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Set".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    pub async fn smembers_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Array(Vec::new()));
        };
        let entry = entry_ref.value();
        if entry.is_expired() {
            drop(entry_ref);
            self.data.remove(key);
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            return Ok(DataType::Array(Vec::new()));
        }

        if let DataType::Set(set) = &entry.value {
            let items = set
                .iter()
                .map(|v| DataType::string_from(v.key().clone()))
                .collect::<Vec<_>>();
            self.touch_key(key).await;
            Ok(DataType::Array(items))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Set".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    pub async fn sismember_cmd(&self, key: &str, member: &[u8]) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_ref.value();
        if entry.is_expired() {
            drop(entry_ref);
            self.data.remove(key);
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            return Ok(DataType::Integer(0));
        }

        if let DataType::Set(set) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Integer(if set.contains(member) { 1 } else { 0 }))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Set".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    pub async fn scard_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_ref.value();
        if entry.is_expired() {
            drop(entry_ref);
            self.data.remove(key);
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            return Ok(DataType::Integer(0));
        }

        if let DataType::Set(set) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Integer(set.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Set".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    pub async fn spop_cmd(&self, key: &str, count: Option<usize>) -> Result<DataType, CacheError> {
        let Some(mut entry_mut) = self.data.get_mut(key) else {
            return Ok(match count {
                Some(_) => DataType::Array(Vec::new()),
                None => DataType::Nil,
            });
        };
        let entry = entry_mut.value_mut();

        if let DataType::Set(set) = &mut entry.value {
            let n = count.unwrap_or(1);
            let mut popped = Vec::new();

            for member in set
                .iter()
                .map(|v| v.key().clone())
                .take(n)
                .collect::<Vec<_>>()
            {
                if set.remove(&member).is_some() {
                    popped.push(member);
                }
            }

            self.touch_key(key).await;

            if count.is_some() {
                Ok(DataType::Array(
                    popped
                        .into_iter()
                        .map(DataType::string_from)
                        .collect::<Vec<_>>(),
                ))
            } else if let Some(one) = popped.into_iter().next() {
                Ok(DataType::string_from(one))
            } else {
                Ok(DataType::Nil)
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Set".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    pub async fn srandmember_cmd(
        &self,
        key: &str,
        count: Option<isize>,
    ) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(match count {
                Some(_) => DataType::Array(Vec::new()),
                None => DataType::Nil,
            });
        };
        let entry = entry_ref.value();

        if let DataType::Set(set) = &entry.value {
            let members = set.iter().map(|v| v.key().clone()).collect::<Vec<_>>();
            self.touch_key(key).await;

            match count {
                None => Ok(members
                    .into_iter()
                    .next()
                    .map(DataType::string_from)
                    .unwrap_or(DataType::Nil)),
                Some(c) if c >= 0 => {
                    let n = c as usize;
                    Ok(DataType::Array(
                        members
                            .into_iter()
                            .take(n)
                            .map(DataType::string_from)
                            .collect::<Vec<_>>(),
                    ))
                }
                Some(c) => {
                    let n = c.unsigned_abs();
                    if members.is_empty() {
                        return Ok(DataType::Array(Vec::new()));
                    }
                    let mut out = Vec::with_capacity(n);
                    let pool = members;
                    for i in 0..n {
                        let idx = i % pool.len();
                        out.push(DataType::string_from(pool[idx].clone()));
                    }
                    Ok(DataType::Array(out))
                }
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "Set".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    // Sorted Set (ZSET) commands

    /// Command: ZADD – Add/update members with scores honoring NX/XX/GT/LT/CH/INCR flags.
    #[allow(clippy::too_many_arguments)]
    pub async fn zadd_cmd(
        &self,
        key: &str,
        nx: bool,
        xx: bool,
        gt: bool,
        lt: bool,
        ch: bool,
        incr: bool,
        members: Vec<(f64, Vec<u8>)>,
    ) -> Result<DataType, CacheError> {
        // Reject any NaN score up front (matches Redis "not a valid float").
        for (score, _) in &members {
            if score.is_nan() {
                return Err(CacheError::InvalidScoreFormat);
            }
        }

        // XX on a missing key is a no-op: never create the key.
        if xx && !self.data.contains_key(key) {
            return Ok(if incr {
                DataType::Nil
            } else {
                DataType::Integer(0)
            });
        }

        let mut entry_mut = self
            .data
            .entry(key.to_string())
            .or_insert_with(|| CacheEntry::new(DataType::SortedSet(SortedSetData::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::SortedSet(ss) = &mut entry.value {
            if incr {
                // INCR mode: exactly one (score, member) pair, treated as an
                // increment of the member's current score.
                let (increment, member) = match members.into_iter().next() {
                    Some(pair) => pair,
                    None => {
                        return Err(CacheError::InvalidCommandArguments(
                            "ERR INCR option supports a single increment-element pair".to_string(),
                        ))
                    }
                };

                let existing = ss.score(&member);
                // Apply conditional flags; on block return Nil.
                if nx && existing.is_some() {
                    return Ok(DataType::Nil);
                }
                if xx && existing.is_none() {
                    return Ok(DataType::Nil);
                }
                let new_score = existing.unwrap_or(0.0) + increment;
                if new_score.is_nan() {
                    // e.g. +inf + -inf produces NaN -> Redis errors.
                    return Err(CacheError::InvalidScoreFormat);
                }
                if let Some(old) = existing {
                    if gt && new_score <= old {
                        return Ok(DataType::Nil);
                    }
                    if lt && new_score >= old {
                        return Ok(DataType::Nil);
                    }
                }
                ss.insert(member, new_score);
                self.touch_key(key).await;
                Ok(DataType::Double(new_score))
            } else {
                let mut added = 0_i64;
                let mut changed = 0_i64;
                for (score, member) in members {
                    match ss.score(&member) {
                        Some(old) => {
                            // Existing member.
                            if nx {
                                continue; // NX only adds new members.
                            }
                            if gt && score <= old {
                                continue;
                            }
                            if lt && score >= old {
                                continue;
                            }
                            if score.to_bits() != old.to_bits() {
                                ss.insert(member, score);
                                changed += 1;
                            }
                        }
                        None => {
                            // Absent member: XX would have blocked creation,
                            // but per-member XX also means do not add.
                            if xx {
                                continue;
                            }
                            // GT/LT only apply to existing members; an absent
                            // member is still added.
                            ss.insert(member, score);
                            added += 1;
                        }
                    }
                }
                self.touch_key(key).await;
                if ch {
                    Ok(DataType::Integer(added + changed))
                } else {
                    Ok(DataType::Integer(added))
                }
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZSCORE – Score of a member, or Nil if absent.
    pub async fn zscore_cmd(&self, key: &str, member: &[u8]) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Nil);
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            match ss.score(member) {
                Some(score) => Ok(DataType::Double(score)),
                None => Ok(DataType::Nil),
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZMSCORE – Scores for several members (Double or Nil per member).
    pub async fn zmscore_cmd(
        &self,
        key: &str,
        members: &[Vec<u8>],
    ) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Array(vec![DataType::Nil; members.len()]));
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            let results = members
                .iter()
                .map(|m| match ss.score(m) {
                    Some(score) => DataType::Double(score),
                    None => DataType::Nil,
                })
                .collect::<Vec<_>>();
            Ok(DataType::Array(results))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZCARD – Number of members in the sorted set.
    pub async fn zcard_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Integer(ss.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZINCRBY – Increment a member's score (creating the key if needed).
    pub async fn zincrby_cmd(
        &self,
        key: &str,
        increment: f64,
        member: Vec<u8>,
    ) -> Result<DataType, CacheError> {
        if increment.is_nan() {
            return Err(CacheError::InvalidScoreFormat);
        }

        let mut entry_mut = self
            .data
            .entry(key.to_string())
            .or_insert_with(|| CacheEntry::new(DataType::SortedSet(SortedSetData::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::SortedSet(ss) = &mut entry.value {
            let new_score = ss.score(&member).unwrap_or(0.0) + increment;
            if new_score.is_nan() {
                return Err(CacheError::InvalidScoreFormat);
            }
            ss.insert(member, new_score);
            self.touch_key(key).await;
            Ok(DataType::Double(new_score))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZREM – Remove members; deletes the key when it becomes empty.
    pub async fn zrem_cmd(&self, key: &str, members: &[Vec<u8>]) -> Result<DataType, CacheError> {
        let Some(mut entry_mut) = self.data.get_mut(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_mut.value_mut();

        if let DataType::SortedSet(ss) = &mut entry.value {
            let mut removed = 0_i64;
            for member in members {
                if ss.remove(member) {
                    removed += 1;
                }
            }
            let now_empty = ss.is_empty();
            self.touch_key(key).await;
            if now_empty {
                drop(entry_mut);
                self.delete(key).await;
            }
            Ok(DataType::Integer(removed))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZRANK / ZREVRANK – Rank of a member, optionally with its score.
    pub async fn zrank_cmd(
        &self,
        key: &str,
        member: &[u8],
        rev: bool,
        with_score: bool,
    ) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Nil);
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            let rank = if rev {
                ss.rev_rank(member)
            } else {
                ss.rank(member)
            };
            match rank {
                Some(r) => {
                    if with_score {
                        let score = ss.score(member).unwrap_or(0.0);
                        Ok(DataType::Array(vec![
                            DataType::Integer(r as i64),
                            DataType::Double(score),
                        ]))
                    } else {
                        Ok(DataType::Integer(r as i64))
                    }
                }
                None => Ok(DataType::Nil),
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    // GEO commands
    //
    // GEO points are stored in an ordinary sorted set whose member score is the
    // 52-bit geohash (as an `f64`). See [`crate::store::geo`] (wired in above as
    // the local `geo` module) for the encode/decode/search primitives.

    /// Format a coordinate component with the 17-digit precision Redis uses for
    /// GEO longitude/latitude bulk-string replies.
    #[inline]
    fn geo_format_coord(v: f64) -> DataType {
        DataType::String(format!("{:.17}", v).into_bytes())
    }

    /// Decode a member's stored sorted-set score into its `(lon, lat)` cell
    /// center; `None` if the member is absent.
    #[inline]
    fn geo_member_lonlat(ss: &SortedSetData, member: &[u8]) -> Option<(f64, f64)> {
        ss.score(member).map(|score| geo::decode(score as u64))
    }

    /// Command: GEOADD – Add geospatial members to the sorted set at `key`.
    /// Returns the number of newly added members (or, with `ch`, the number of
    /// members added or whose position changed).
    pub async fn geoadd_cmd(
        &self,
        key: &str,
        nx: bool,
        xx: bool,
        ch: bool,
        items: Vec<(f64, f64, Vec<u8>)>,
    ) -> Result<DataType, CacheError> {
        // Validate all coordinates up front so an invalid pair never partially
        // mutates the set.
        let mut encoded: Vec<(u64, Vec<u8>)> = Vec::with_capacity(items.len());
        for (lon, lat, member) in items {
            let bits = geo::encode(lon, lat).ok_or_else(|| {
                CacheError::InvalidCommandArguments("ERR invalid longitude,latitude pair".into())
            })?;
            encoded.push((bits, member));
        }

        // XX on a missing key is a no-op: never create the key.
        if xx && !self.data.contains_key(key) {
            return Ok(DataType::Integer(0));
        }

        let mut entry_mut = self
            .data
            .entry(key.to_string())
            .or_insert_with(|| CacheEntry::new(DataType::SortedSet(SortedSetData::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::SortedSet(ss) = &mut entry.value {
            let mut added = 0_i64;
            let mut changed = 0_i64;
            for (bits, member) in encoded {
                let score = bits as f64;
                match ss.score(&member) {
                    Some(old) => {
                        if nx {
                            continue; // NX only adds new members.
                        }
                        if score.to_bits() != old.to_bits() {
                            ss.insert(member, score);
                            changed += 1;
                        }
                    }
                    None => {
                        if xx {
                            continue; // XX only updates existing members.
                        }
                        ss.insert(member, score);
                        added += 1;
                    }
                }
            }
            self.touch_key(key).await;
            if ch {
                Ok(DataType::Integer(added + changed))
            } else {
                Ok(DataType::Integer(added))
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: GEOPOS – Positions (lon/lat) of members, or Nil per missing one.
    pub async fn geopos_cmd(&self, key: &str, members: &[Vec<u8>]) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Array(vec![DataType::Nil; members.len()]));
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            let results = members
                .iter()
                .map(|m| match Self::geo_member_lonlat(ss, m) {
                    Some((lon, lat)) => DataType::Array(vec![
                        Self::geo_format_coord(lon),
                        Self::geo_format_coord(lat),
                    ]),
                    None => DataType::Nil,
                })
                .collect::<Vec<_>>();
            Ok(DataType::Array(results))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: GEODIST – Distance between two members, scaled by `unit_div`
    /// (meters per requested unit), as a 4-decimal bulk string; Nil if either
    /// member is missing.
    pub async fn geodist_cmd(
        &self,
        key: &str,
        m1: &[u8],
        m2: &[u8],
        unit_div: f64,
    ) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Nil);
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            let (Some((lon1, lat1)), Some((lon2, lat2))) = (
                Self::geo_member_lonlat(ss, m1),
                Self::geo_member_lonlat(ss, m2),
            ) else {
                return Ok(DataType::Nil);
            };
            let meters = geo::haversine_m(lon1, lat1, lon2, lat2);
            Ok(DataType::String(
                format!("{:.4}", meters / unit_div).into_bytes(),
            ))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: GEOHASH – 11-character base32 geohash of members, Nil if absent.
    pub async fn geohash_cmd(
        &self,
        key: &str,
        members: &[Vec<u8>],
    ) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Array(vec![DataType::Nil; members.len()]));
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            let results = members
                .iter()
                .map(|m| match Self::geo_member_lonlat(ss, m) {
                    Some((lon, lat)) => {
                        DataType::String(geo::geohash_string(lon, lat).into_bytes())
                    }
                    None => DataType::Nil,
                })
                .collect::<Vec<_>>();
            Ok(DataType::Array(results))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: GEOSEARCH – Members within `radius_m` meters of a center
    /// (given directly via `from_lonlat` or resolved from `from_member`).
    /// Optional `with_*` flags add coordinates, distance and/or raw hash to each
    /// reply element; `asc` controls sort direction; `count` limits results.
    #[allow(clippy::too_many_arguments)]
    pub async fn geosearch_cmd(
        &self,
        key: &str,
        from_member: Option<&[u8]>,
        from_lonlat: Option<(f64, f64)>,
        radius_m: f64,
        unit_div: f64,
        with_coord: bool,
        with_dist: bool,
        with_hash: bool,
        count: Option<usize>,
        asc: Option<bool>,
    ) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Array(Vec::new()));
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            // Determine the search center.
            let (center_lon, center_lat) = match from_lonlat {
                Some(c) => c,
                None => {
                    let member = from_member.ok_or_else(|| {
                        CacheError::InvalidCommandArguments(
                            "ERR could not decode requested zset member".into(),
                        )
                    })?;
                    Self::geo_member_lonlat(ss, member).ok_or_else(|| {
                        CacheError::InvalidCommandArguments(
                            "ERR could not decode requested zset member".into(),
                        )
                    })?
                }
            };

            self.touch_key(key).await;

            // Collect candidates from each covering score range, deduplicating
            // members that appear in more than one range.
            let ranges = geo::search_ranges(center_lon, center_lat, radius_m);
            let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
            // (member, distance_m, lon, lat, score)
            let mut results: Vec<(Vec<u8>, f64, f64, f64, f64)> = Vec::new();
            for (min, max) in ranges {
                let candidates = ss.range_by_score(
                    ScoreBound::Inclusive(min as f64),
                    ScoreBound::Inclusive(max as f64),
                    false,
                    0,
                    None,
                );
                for (member, score) in candidates {
                    if !seen.insert(member.clone()) {
                        continue;
                    }
                    let (lon, lat) = geo::decode(score as u64);
                    let d = geo::haversine_m(center_lon, center_lat, lon, lat);
                    if d <= radius_m {
                        results.push((member, d, lon, lat, score));
                    }
                }
            }

            // Sort by distance per ASC/DESC; leave unsorted when neither given.
            match asc {
                Some(true) => results.sort_by(|a, b| a.1.total_cmp(&b.1)),
                Some(false) => results.sort_by(|a, b| b.1.total_cmp(&a.1)),
                None => {}
            }

            if let Some(limit) = count {
                results.truncate(limit);
            }

            let with_extras = with_coord || with_dist || with_hash;
            let reply = results
                .into_iter()
                .map(|(member, d, lon, lat, score)| {
                    if !with_extras {
                        DataType::String(member)
                    } else {
                        // Redis order: member, dist, hash, coords.
                        let mut element = vec![DataType::String(member)];
                        if with_dist {
                            element.push(DataType::String(
                                format!("{:.4}", d / unit_div).into_bytes(),
                            ));
                        }
                        if with_hash {
                            element.push(DataType::Integer(score as i64));
                        }
                        if with_coord {
                            element.push(DataType::Array(vec![
                                Self::geo_format_coord(lon),
                                Self::geo_format_coord(lat),
                            ]));
                        }
                        DataType::Array(element)
                    }
                })
                .collect::<Vec<_>>();
            Ok(DataType::Array(reply))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZRANGE / ZREVRANGE (by index) – Members within an index range.
    pub async fn zrange_cmd(
        &self,
        key: &str,
        start: i64,
        stop: i64,
        rev: bool,
        with_scores: bool,
    ) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Array(Vec::new()));
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            let items = ss.range_by_index(start, stop, rev);
            Ok(DataType::Array(Self::zset_pairs_to_reply(
                items,
                with_scores,
            )))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZRANGEBYSCORE / ZREVRANGEBYSCORE – Members within a score range.
    /// `min` is the low bound, `max` the high bound (caller swaps for REV).
    pub async fn zrangebyscore_cmd(
        &self,
        key: &str,
        min: &str,
        max: &str,
        rev: bool,
        with_scores: bool,
        limit: Option<(i64, i64)>,
    ) -> Result<DataType, CacheError> {
        let min_bound = parse_score_bound(min).map_err(|_| CacheError::InvalidScoreFormat)?;
        let max_bound = parse_score_bound(max).map_err(|_| CacheError::InvalidScoreFormat)?;
        let (offset, count) = Self::normalize_zset_limit(limit);

        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Array(Vec::new()));
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            let items = ss.range_by_score(min_bound, max_bound, rev, offset, count);
            Ok(DataType::Array(Self::zset_pairs_to_reply(
                items,
                with_scores,
            )))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZRANGEBYLEX / ZREVRANGEBYLEX – Members within a lex range.
    pub async fn zrangebylex_cmd(
        &self,
        key: &str,
        min: &str,
        max: &str,
        rev: bool,
        limit: Option<(i64, i64)>,
    ) -> Result<DataType, CacheError> {
        let min_bound = parse_lex_bound(min).map_err(|_| {
            CacheError::InvalidCommandArguments("min or max not valid string range item".into())
        })?;
        let max_bound = parse_lex_bound(max).map_err(|_| {
            CacheError::InvalidCommandArguments("min or max not valid string range item".into())
        })?;
        let (offset, count) = Self::normalize_zset_limit(limit);

        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Array(Vec::new()));
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            let members = ss.range_by_lex(min_bound, max_bound, rev, offset, count);
            Ok(DataType::Array(
                members.into_iter().map(DataType::string_from).collect(),
            ))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZCOUNT – Count members within a score range.
    pub async fn zcount_cmd(
        &self,
        key: &str,
        min: &str,
        max: &str,
    ) -> Result<DataType, CacheError> {
        let min_bound = parse_score_bound(min).map_err(|_| CacheError::InvalidScoreFormat)?;
        let max_bound = parse_score_bound(max).map_err(|_| CacheError::InvalidScoreFormat)?;

        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Integer(
                ss.count_by_score(min_bound, max_bound) as i64
            ))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZLEXCOUNT – Count members within a lex range.
    pub async fn zlexcount_cmd(
        &self,
        key: &str,
        min: &str,
        max: &str,
    ) -> Result<DataType, CacheError> {
        let min_bound = parse_lex_bound(min).map_err(|_| {
            CacheError::InvalidCommandArguments("min or max not valid string range item".into())
        })?;
        let max_bound = parse_lex_bound(max).map_err(|_| {
            CacheError::InvalidCommandArguments("min or max not valid string range item".into())
        })?;

        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Integer(
                ss.count_by_lex(min_bound, max_bound) as i64
            ))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZPOPMIN – Remove & return up to `count` lowest-scored members.
    pub async fn zpopmin_cmd(
        &self,
        key: &str,
        count: Option<usize>,
    ) -> Result<DataType, CacheError> {
        let Some(mut entry_mut) = self.data.get_mut(key) else {
            return Ok(DataType::Array(Vec::new()));
        };
        let entry = entry_mut.value_mut();

        if let DataType::SortedSet(ss) = &mut entry.value {
            let popped = ss.pop_min(count.unwrap_or(1));
            let now_empty = ss.is_empty();
            self.touch_key(key).await;
            let reply = Self::zset_pairs_to_reply(popped, true);
            if now_empty {
                drop(entry_mut);
                self.delete(key).await;
            }
            Ok(DataType::Array(reply))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZPOPMAX – Remove & return up to `count` highest-scored members.
    pub async fn zpopmax_cmd(
        &self,
        key: &str,
        count: Option<usize>,
    ) -> Result<DataType, CacheError> {
        let Some(mut entry_mut) = self.data.get_mut(key) else {
            return Ok(DataType::Array(Vec::new()));
        };
        let entry = entry_mut.value_mut();

        if let DataType::SortedSet(ss) = &mut entry.value {
            let popped = ss.pop_max(count.unwrap_or(1));
            let now_empty = ss.is_empty();
            self.touch_key(key).await;
            let reply = Self::zset_pairs_to_reply(popped, true);
            if now_empty {
                drop(entry_mut);
                self.delete(key).await;
            }
            Ok(DataType::Array(reply))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZREMRANGEBYRANK – Remove members within an index range.
    pub async fn zremrangebyrank_cmd(
        &self,
        key: &str,
        start: i64,
        stop: i64,
    ) -> Result<DataType, CacheError> {
        let Some(mut entry_mut) = self.data.get_mut(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_mut.value_mut();

        if let DataType::SortedSet(ss) = &mut entry.value {
            let removed = ss.remove_range_by_rank(start, stop);
            let now_empty = ss.is_empty();
            self.touch_key(key).await;
            if now_empty {
                drop(entry_mut);
                self.delete(key).await;
            }
            Ok(DataType::Integer(removed as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZREMRANGEBYSCORE – Remove members within a score range.
    pub async fn zremrangebyscore_cmd(
        &self,
        key: &str,
        min: &str,
        max: &str,
    ) -> Result<DataType, CacheError> {
        let min_bound = parse_score_bound(min).map_err(|_| CacheError::InvalidScoreFormat)?;
        let max_bound = parse_score_bound(max).map_err(|_| CacheError::InvalidScoreFormat)?;

        let Some(mut entry_mut) = self.data.get_mut(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_mut.value_mut();

        if let DataType::SortedSet(ss) = &mut entry.value {
            let removed = ss.remove_range_by_score(min_bound, max_bound);
            let now_empty = ss.is_empty();
            self.touch_key(key).await;
            if now_empty {
                drop(entry_mut);
                self.delete(key).await;
            }
            Ok(DataType::Integer(removed as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZREMRANGEBYLEX – Remove members within a lex range.
    pub async fn zremrangebylex_cmd(
        &self,
        key: &str,
        min: &str,
        max: &str,
    ) -> Result<DataType, CacheError> {
        let min_bound = parse_lex_bound(min).map_err(|_| {
            CacheError::InvalidCommandArguments("min or max not valid string range item".into())
        })?;
        let max_bound = parse_lex_bound(max).map_err(|_| {
            CacheError::InvalidCommandArguments("min or max not valid string range item".into())
        })?;

        let Some(mut entry_mut) = self.data.get_mut(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_mut.value_mut();

        if let DataType::SortedSet(ss) = &mut entry.value {
            let removed = ss.remove_range_by_lex(min_bound, max_bound);
            let now_empty = ss.is_empty();
            self.touch_key(key).await;
            if now_empty {
                drop(entry_mut);
                self.delete(key).await;
            }
            Ok(DataType::Integer(removed as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZRANDMEMBER – Random member(s), optionally with scores.
    pub async fn zrandmember_cmd(
        &self,
        key: &str,
        count: Option<i64>,
        with_scores: bool,
    ) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(match count {
                Some(_) => DataType::Array(Vec::new()),
                None => DataType::Nil,
            });
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            match count {
                None => {
                    // Single random member as a bulk string (or Nil if empty).
                    let picked = ss.random_members(1);
                    match picked.into_iter().next() {
                        Some((member, _)) => Ok(DataType::string_from(member)),
                        None => Ok(DataType::Nil),
                    }
                }
                Some(c) => {
                    let items = ss.random_members(c);
                    Ok(DataType::Array(Self::zset_pairs_to_reply(
                        items,
                        with_scores,
                    )))
                }
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: ZSCAN – Cursor-style scan of members (single-pass, returns "0").
    pub async fn zscan_cmd(
        &self,
        key: &str,
        _cursor: u64,
        pattern: Option<String>,
        _count: Option<usize>,
    ) -> Result<DataType, CacheError> {
        let empty_reply = || {
            DataType::Array(vec![
                DataType::string_from("0"),
                DataType::Array(Vec::new()),
            ])
        };

        let Some(entry_ref) = self.data.get(key) else {
            return Ok(empty_reply());
        };
        let entry = entry_ref.value();

        if let DataType::SortedSet(ss) = &entry.value {
            self.touch_key(key).await;
            let mut flat: Vec<DataType> = Vec::new();
            for (member, score) in ss.entries() {
                if let Some(ref pat) = pattern {
                    let text = String::from_utf8_lossy(&member);
                    if !glob_matches(pat, &text) {
                        continue;
                    }
                }
                flat.push(DataType::string_from(member));
                flat.push(DataType::Double(score));
            }
            Ok(DataType::Array(vec![
                DataType::string_from("0"),
                DataType::Array(flat),
            ]))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "zset".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Convert a `(member, score)` list into a flat reply array. When
    /// `with_scores` is set, each member is followed by its score; otherwise only
    /// members are emitted.
    fn zset_pairs_to_reply(pairs: Vec<(Vec<u8>, f64)>, with_scores: bool) -> Vec<DataType> {
        if with_scores {
            let mut out = Vec::with_capacity(pairs.len() * 2);
            for (member, score) in pairs {
                out.push(DataType::string_from(member));
                out.push(DataType::Double(score));
            }
            out
        } else {
            pairs
                .into_iter()
                .map(|(member, _)| DataType::string_from(member))
                .collect()
        }
    }

    /// Normalize a Redis `LIMIT offset count` (both `i64`) into
    /// `(offset: usize, count: Option<usize>)`. Negative offset clamps to 0; a
    /// negative count means "no limit" (`None`).
    fn normalize_zset_limit(limit: Option<(i64, i64)>) -> (usize, Option<usize>) {
        match limit {
            Some((offset, count)) => {
                let offset = if offset < 0 { 0 } else { offset as usize };
                let count = if count < 0 {
                    None
                } else {
                    Some(count as usize)
                };
                (offset, count)
            }
            None => (0, None),
        }
    }

    #[inline]
    /// Remove a specific key hash from the LRU (used post-eviction / deletion).
    pub async fn remove_from_lru_with_hash(&self, key_hash: u64) {
        if let Ok(mut lru) = self.lru_cache.try_write() {
            lru.pop(&key_hash);
        }
    }

    /// Remove a key from the LRU
    #[inline]
    pub async fn remove_from_lru(&self, key: &str) {
        let hash = Self::stable_hash_key(key);
        self.remove_from_lru_with_hash(hash).await;
    }

    /// Perform eviction when over capacity using candidate ranking.
    pub async fn evict(&self) {
        use std::collections::BinaryHeap;

        let target_size = (self.capacity.load(Ordering::Relaxed) as f64 * 0.8) as usize;
        let current_size = self.size.load(Ordering::Relaxed);
        if current_size <= target_size {
            return;
        }

        let mut candidates = BinaryHeap::new();
        const MAX_CANDIDATES: usize = 50;

        if let Ok(lru) = self.lru_cache.try_read() {
            let iter = lru.iter().rev();
            for (key_hash, _) in iter.take(MAX_CANDIDATES) {
                // Find the actual key for this hash
                for entry_ref in self.data.iter() {
                    if Self::stable_hash_key(entry_ref.key()) == *key_hash {
                        let score = entry_ref.value().eviction_score();
                        candidates.push(EvictionCandidate {
                            key: entry_ref.key().clone(),
                            score,
                        });
                        break;
                    }
                }
            }
        }

        // Evict entries until we reach target size (re-evaluating size each iteration)
        while self.size.load(Ordering::Relaxed) > target_size && !candidates.is_empty() {
            if let Some(candidate) = candidates.pop() {
                if let Some((_, entry)) = self.data.remove(&candidate.key) {
                    let entry_size = entry.size();
                    self.size.fetch_sub(1, Ordering::Relaxed);
                    self.metrics
                        .total_data_size
                        .fetch_sub(entry_size as u64, Ordering::Relaxed);
                    self.metrics.record_eviction();
                    self.remove_from_lru(&candidate.key).await;
                }
            }
        }
    }

    // Bitmap Commands (operate on the bytes of a DataType::String)

    /// Command: SETBIT – Set or clear the bit at `offset` (MSB-first within each
    /// byte, like Redis). Grows the underlying string with zero bytes as needed.
    /// Returns the previous bit value as an Integer.
    pub async fn setbit_cmd(
        &self,
        key: &str,
        offset: u64,
        value: u8,
    ) -> Result<DataType, CacheError> {
        // Redis rejects offsets >= 2^32 (4 * 1024 * 1024 * 1024 bits).
        if offset >= (1u64 << 32) {
            return Err(CacheError::InvalidCommandArguments(
                "ERR bit offset is not an integer or out of range".into(),
            ));
        }

        let byte_index = (offset / 8) as usize;
        let bit_index = 7 - (offset % 8) as u8;
        let needed_len = byte_index + 1;

        // Get or create entry as a string, mirroring setrange_cmd.
        let mut entry_writer = match self.data.get_mut(key) {
            Some(writer) => writer,
            None => {
                let new_string = vec![0u8; needed_len];
                self.set(key.to_string(), DataType::String(new_string), None)
                    .await;
                self.data.get_mut(key).unwrap() // Safe since we just inserted it
            }
        };

        let entry = entry_writer.value_mut();

        if let DataType::String(s) = &mut entry.value {
            if needed_len > s.len() {
                s.resize(needed_len, 0u8);
            }

            let old_bit = (s[byte_index] >> bit_index) & 1;
            if value != 0 {
                s[byte_index] |= 1 << bit_index;
            } else {
                s[byte_index] &= !(1 << bit_index);
            }

            self.touch_key(key).await;
            Ok(DataType::Integer(old_bit as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "string".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: GETBIT – Return the bit value at `offset` (MSB-first). Missing
    /// key or out-of-range offset yields 0.
    pub async fn getbit_cmd(&self, key: &str, offset: u64) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_ref.value();

        if entry.is_expired() {
            drop(entry_ref);
            self.data.remove(key);
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            return Ok(DataType::Integer(0));
        }

        if let DataType::String(s) = &entry.value {
            let byte_index = (offset / 8) as usize;
            let bit_index = 7 - (offset % 8) as u8;
            let bit = if byte_index < s.len() {
                (s[byte_index] >> bit_index) & 1
            } else {
                0
            };
            self.touch_key(key).await;
            Ok(DataType::Integer(bit as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "string".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: BITCOUNT – Count set bits in the string, optionally restricted to
    /// a BYTE range (`bit_unit == false`) or BIT range (`bit_unit == true`).
    /// Missing key yields 0.
    pub async fn bitcount_cmd(
        &self,
        key: &str,
        range: Option<(i64, i64)>,
        bit_unit: bool,
    ) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_ref.value();

        if entry.is_expired() {
            drop(entry_ref);
            self.data.remove(key);
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            return Ok(DataType::Integer(0));
        }

        if let DataType::String(s) = &entry.value {
            self.touch_key(key).await;

            let count: i64 = match range {
                None => s.iter().map(|b| b.count_ones() as i64).sum(),
                Some((start, end)) if !bit_unit => {
                    let len = s.len() as i64;
                    let (start_idx, end_idx) = Self::normalize_range(start, end, len);
                    if start_idx > end_idx {
                        0
                    } else {
                        s[start_idx as usize..=end_idx as usize]
                            .iter()
                            .map(|b| b.count_ones() as i64)
                            .sum()
                    }
                }
                Some((start, end)) => {
                    let total_bits = (s.len() as i64) * 8;
                    let (start_bit, end_bit) = Self::normalize_range(start, end, total_bits);
                    if start_bit > end_bit {
                        0
                    } else {
                        let mut count = 0i64;
                        for bit_pos in start_bit..=end_bit {
                            let byte_index = (bit_pos / 8) as usize;
                            let bit_index = 7 - (bit_pos % 8) as u8;
                            if (s[byte_index] >> bit_index) & 1 == 1 {
                                count += 1;
                            }
                        }
                        count
                    }
                }
            };

            Ok(DataType::Integer(count))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "string".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Normalize an inclusive `[start, end]` range over `len` units, translating
    /// negative indices from the end and clamping into `[0, len-1]`. Returns
    /// `(start, end)` where `start > end` signals an empty range.
    fn normalize_range(start: i64, end: i64, len: i64) -> (i64, i64) {
        if len == 0 {
            return (0, -1);
        }
        let mut start_idx = if start < 0 { start + len } else { start };
        let mut end_idx = if end < 0 { end + len } else { end };
        if start_idx < 0 {
            start_idx = 0;
        }
        if end_idx >= len {
            end_idx = len - 1;
        }
        (start_idx, end_idx)
    }

    /// Command: BITPOS – Return the position of the first bit set to `bit` (0/1),
    /// optionally restricted by `start`/`end` over BYTE ranges (default) or BIT
    /// ranges (`bit_unit == true`). Follows Redis edge-case semantics.
    pub async fn bitpos_cmd(
        &self,
        key: &str,
        bit: u8,
        start: Option<i64>,
        end: Option<i64>,
        bit_unit: bool,
    ) -> Result<DataType, CacheError> {
        let bit = if bit != 0 { 1u8 } else { 0u8 };

        let Some(entry_ref) = self.data.get(key) else {
            // Missing key: searching for 0 returns 0, for 1 returns -1.
            return Ok(DataType::Integer(if bit == 0 { 0 } else { -1 }));
        };
        let entry = entry_ref.value();

        if entry.is_expired() {
            drop(entry_ref);
            self.data.remove(key);
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            return Ok(DataType::Integer(if bit == 0 { 0 } else { -1 }));
        }

        if let DataType::String(s) = &entry.value {
            self.touch_key(key).await;

            let len = s.len() as i64;
            if len == 0 {
                return Ok(DataType::Integer(if bit == 0 { 0 } else { -1 }));
            }

            // Whether the user explicitly supplied an end bound (affects the
            // "searching for 0 with no end" Redis quirk).
            let end_specified = end.is_some();

            // Compute the inclusive bit range [start_bit, end_bit].
            let (start_bit, end_bit) = if bit_unit {
                let total_bits = len * 8;
                let s_arg = start.unwrap_or(0);
                let e_arg = end.unwrap_or(total_bits - 1);
                Self::normalize_range(s_arg, e_arg, total_bits)
            } else {
                let s_arg = start.unwrap_or(0);
                let e_arg = end.unwrap_or(len - 1);
                let (start_byte, end_byte) = Self::normalize_range(s_arg, e_arg, len);
                if start_byte > end_byte {
                    (1, 0) // empty range sentinel
                } else {
                    (start_byte * 8, end_byte * 8 + 7)
                }
            };

            if start_bit > end_bit {
                return Ok(DataType::Integer(-1));
            }

            for bit_pos in start_bit..=end_bit {
                let byte_index = (bit_pos / 8) as usize;
                let bit_index = 7 - (bit_pos % 8) as u8;
                let cur = (s[byte_index] >> bit_index) & 1;
                if cur == bit {
                    return Ok(DataType::Integer(bit_pos));
                }
            }

            // No matching bit in the searched range.
            // Redis quirk: when searching for a 0, the string is all 1s in the
            // range, and no explicit end was given, the result is the first bit
            // past the end of the string (len * 8).
            if bit == 0 && !end_specified {
                Ok(DataType::Integer(len * 8))
            } else {
                Ok(DataType::Integer(-1))
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "string".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    // HyperLogLog Commands

    /// Command: PFADD – Add `elements` to the HyperLogLog stored at `key`,
    /// creating it if absent. Returns 1 if the sketch was altered or the key was
    /// newly created, 0 otherwise.
    pub async fn pfadd_cmd(&self, key: &str, elements: &[Vec<u8>]) -> Result<DataType, CacheError> {
        // Determine the current sketch and whether the key is being created.
        let (mut hll, mut changed) = if let Some(entry_ref) = self.data.get(key) {
            let entry = entry_ref.value();
            match &entry.value {
                DataType::String(s) => {
                    if Hll::is_hll(s) {
                        // Safe: is_hll just validated the layout.
                        (Hll::from_bytes(s).unwrap(), false)
                    } else if s.is_empty() {
                        (Hll::new(), true)
                    } else {
                        return Err(CacheError::InvalidCommandArguments(
                            "WRONGTYPE Key is not a valid HyperLogLog string value.".into(),
                        ));
                    }
                }
                _ => {
                    return Err(CacheError::DataTypeMismatch(
                        key.to_string(),
                        "string".to_string(),
                        entry.value.data_type(),
                    ));
                }
            }
        } else {
            // Missing key: PFADD always creates a new HLL (returns 1 even with
            // zero elements).
            (Hll::new(), true)
        };

        for element in elements {
            if hll.add(element) {
                changed = true;
            }
        }

        if changed {
            self.set(key.to_string(), DataType::String(hll.to_bytes()), None)
                .await;
            Ok(DataType::Integer(1))
        } else {
            Ok(DataType::Integer(0))
        }
    }

    // Stream commands

    /// Build a Redis "entries reply" from a list of `(id, fields)` pairs.
    ///
    /// Each entry renders as `[ "ms-seq", ["f1","v1","f2","v2",...] ]`; the
    /// outer reply is the array of those entries.
    fn stream_entries_reply(entries: Vec<stream::StreamEntry>) -> DataType {
        let elems = entries
            .into_iter()
            .map(|(id, fields)| {
                let mut flat: Vec<DataType> = Vec::with_capacity(fields.len() * 2);
                for (field, value) in fields {
                    flat.push(DataType::String(field));
                    flat.push(DataType::String(value));
                }
                DataType::Array(vec![
                    DataType::String(id.to_redis_string().into_bytes()),
                    DataType::Array(flat),
                ])
            })
            .collect::<Vec<DataType>>();
        DataType::Array(elems)
    }

    /// Command: XADD – Append an entry to a stream, creating it if absent.
    pub async fn xadd_cmd(
        &self,
        key: &str,
        id: &str,
        nomkstream: bool,
        maxlen: Option<usize>,
        fields: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<DataType, CacheError> {
        // NOMKSTREAM on a missing key is a no-op (null reply).
        if nomkstream && !self.data.contains_key(key) {
            return Ok(DataType::Nil);
        }

        let mut entry_mut = self
            .data
            .entry(key.to_string())
            .or_insert_with(|| CacheEntry::new(DataType::Stream(StreamData::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            // Resolve the requested ID.
            let resolved_id = if id == "*" {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                s.next_auto_id(now_ms)
            } else if id.contains('-') {
                // Fully specified "ms-seq".
                match stream::parse_id(id, 0) {
                    Some(parsed) => parsed,
                    None => {
                        return Err(CacheError::InvalidCommandArguments(
                            "ERR Invalid stream ID specified as stream command argument".into(),
                        ))
                    }
                }
            } else {
                // Only "ms" given: Redis auto-picks the sequence. Use last
                // seq+1 when the ms matches the stream's top item, else seq 0.
                match stream::parse_id(id, 0) {
                    Some(parsed) => {
                        let last = s.last_id();
                        if parsed.ms == last.ms {
                            StreamId {
                                ms: parsed.ms,
                                seq: last.seq + 1,
                            }
                        } else {
                            StreamId {
                                ms: parsed.ms,
                                seq: 0,
                            }
                        }
                    }
                    None => {
                        return Err(CacheError::InvalidCommandArguments(
                            "ERR Invalid stream ID specified as stream command argument".into(),
                        ))
                    }
                }
            };

            let added_id = s.add(resolved_id, fields).map_err(|_| {
                CacheError::InvalidCommandArguments(
                    "ERR The ID specified in XADD is equal or smaller than the target stream top item"
                        .into(),
                )
            })?;

            if let Some(maxlen) = maxlen {
                s.trim_maxlen(maxlen);
            }

            self.touch_key(key).await;
            Ok(DataType::String(added_id.to_redis_string().into_bytes()))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XLEN – Number of entries in a stream (0 if missing).
    pub async fn xlen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_ref.value();

        if let DataType::Stream(s) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Integer(s.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XRANGE / XREVRANGE – Entries within an inclusive ID range.
    /// The caller always supplies the low bound as `start` and the high bound
    /// as `end`; `rev` only controls ordering of the reply.
    pub async fn xrange_cmd(
        &self,
        key: &str,
        start: &str,
        end: &str,
        count: Option<usize>,
        rev: bool,
    ) -> Result<DataType, CacheError> {
        let start_id = stream::parse_range_start(start).ok_or_else(|| {
            CacheError::InvalidCommandArguments(
                "ERR Invalid stream ID specified as stream command argument".into(),
            )
        })?;
        let end_id = stream::parse_range_end(end).ok_or_else(|| {
            CacheError::InvalidCommandArguments(
                "ERR Invalid stream ID specified as stream command argument".into(),
            )
        })?;

        let Some(entry_ref) = self.data.get(key) else {
            return Ok(DataType::Array(Vec::new()));
        };
        let entry = entry_ref.value();

        if let DataType::Stream(s) = &entry.value {
            self.touch_key(key).await;
            let items = s.range(start_id, end_id, rev, count);
            Ok(Self::stream_entries_reply(items))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XDEL – Remove specific entries by ID. Returns the number of
    /// entries actually removed. The (now possibly empty) stream key is kept.
    pub async fn xdel_cmd(&self, key: &str, ids: &[String]) -> Result<DataType, CacheError> {
        // Parse all IDs up front; a malformed ID is an error.
        let mut parsed_ids: Vec<StreamId> = Vec::with_capacity(ids.len());
        for id in ids {
            match stream::parse_id(id, 0) {
                Some(parsed) => parsed_ids.push(parsed),
                None => {
                    return Err(CacheError::InvalidCommandArguments(
                        "ERR Invalid stream ID specified as stream command argument".into(),
                    ))
                }
            }
        }

        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_writer.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            let removed = s.delete(&parsed_ids);
            self.touch_key(key).await;
            Ok(DataType::Integer(removed as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XTRIM – Trim a stream to at most `maxlen` entries. Returns the
    /// number of entries removed.
    pub async fn xtrim_cmd(&self, key: &str, maxlen: usize) -> Result<DataType, CacheError> {
        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_writer.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            let removed = s.trim_maxlen(maxlen);
            self.touch_key(key).await;
            Ok(DataType::Integer(removed as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Read entries newer than `id_token` from a single stream, for `XREAD`.
    ///
    /// Resolves the "after" id (`"$"` -> the stream's current `last_id()`,
    /// `"0"`/`"0-0"` -> the minimum id, otherwise parsed) and returns
    /// `Some([ key, entries_reply ])` when at least one entry matches. Missing
    /// keys yield `None`; a non-stream value is a type mismatch error.
    pub async fn xread_one(
        &self,
        key: &str,
        id_token: &str,
        count: Option<usize>,
    ) -> Result<Option<DataType>, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Ok(None);
        };
        let entry = entry_ref.value();

        if let DataType::Stream(s) = &entry.value {
            let after = if id_token == "$" {
                s.last_id()
            } else if id_token == "0" || id_token == "0-0" {
                StreamId::MIN
            } else {
                match stream::parse_id(id_token, 0) {
                    Some(parsed) => parsed,
                    None => {
                        return Err(CacheError::InvalidCommandArguments(
                            "ERR Invalid stream ID specified as stream command argument".into(),
                        ))
                    }
                }
            };

            let entries = s.read_after(after, count);
            self.touch_key(key).await;
            if entries.is_empty() {
                Ok(None)
            } else {
                Ok(Some(DataType::Array(vec![
                    DataType::String(key.to_string().into_bytes()),
                    Self::stream_entries_reply(entries),
                ])))
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    // Stream consumer-group commands (Phase 8b)

    /// Resolve a group-start id token (`XGROUP CREATE`/`SETID`, `$`-aware).
    ///
    /// `"$"` -> the stream's current `last_id()`, `"0"`/`"0-0"` -> the minimum,
    /// otherwise the parsed id. A malformed id is the standard error.
    fn resolve_group_start(s: &StreamData, id: &str) -> Result<StreamId, CacheError> {
        if id == "$" {
            Ok(s.last_id())
        } else if id == "0" || id == "0-0" {
            Ok(StreamId::MIN)
        } else {
            stream::parse_id(id, 0).ok_or_else(|| {
                CacheError::InvalidCommandArguments(
                    "ERR Invalid stream ID specified as stream command argument".into(),
                )
            })
        }
    }

    /// Build a single stream entry reply element: `[ "ms-seq", [f, v, ...] ]`.
    fn stream_entry_element(entry: stream::StreamEntry) -> DataType {
        let (id, fields) = entry;
        let mut flat: Vec<DataType> = Vec::with_capacity(fields.len() * 2);
        for (field, value) in fields {
            flat.push(DataType::String(field));
            flat.push(DataType::String(value));
        }
        DataType::Array(vec![
            DataType::String(id.to_redis_string().into_bytes()),
            DataType::Array(flat),
        ])
    }

    /// Command: XGROUP CREATE – Create a consumer group on a stream.
    pub async fn xgroup_create_cmd(
        &self,
        key: &str,
        group: &str,
        id: &str,
        mkstream: bool,
    ) -> Result<DataType, CacheError> {
        if !self.data.contains_key(key) && !mkstream {
            return Err(CacheError::InvalidCommandArguments(
                "ERR The XGROUP subcommand requires the key to exist. Note that for CREATE you may want to use the MKSTREAM option to create an empty stream automatically.".into(),
            ));
        }

        let mut entry_mut = self
            .data
            .entry(key.to_string())
            .or_insert_with(|| CacheEntry::new(DataType::Stream(StreamData::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            let start = Self::resolve_group_start(s, id)?;
            if !s.create_group(group, start) {
                return Err(CacheError::InvalidCommandArguments(
                    "BUSYGROUP Consumer Group name already exists".into(),
                ));
            }
            drop(entry_mut);
            self.touch_key(key).await;
            Ok(DataType::Status("OK".into()))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XGROUP SETID – Set a group's `last_delivered_id`.
    pub async fn xgroup_setid_cmd(
        &self,
        key: &str,
        group: &str,
        id: &str,
    ) -> Result<DataType, CacheError> {
        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Err(CacheError::InvalidCommandArguments(
                "ERR The XGROUP subcommand requires the key to exist. Note that for CREATE you may want to use the MKSTREAM option to create an empty stream automatically.".into(),
            ));
        };
        let entry = entry_writer.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            let start = Self::resolve_group_start(s, id)?;
            if !s.set_group_id(group, start) {
                return Err(CacheError::InvalidCommandArguments(format!(
                    "NOGROUP No such key '{}' or consumer group '{}'",
                    key, group
                )));
            }
            drop(entry_writer);
            self.touch_key(key).await;
            Ok(DataType::Status("OK".into()))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XGROUP DESTROY – Remove a consumer group entirely.
    pub async fn xgroup_destroy_cmd(&self, key: &str, group: &str) -> Result<DataType, CacheError> {
        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Err(CacheError::InvalidCommandArguments(
                "ERR The XGROUP subcommand requires the key to exist. Note that for CREATE you may want to use the MKSTREAM option to create an empty stream automatically.".into(),
            ));
        };
        let entry = entry_writer.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            let destroyed = s.destroy_group(group);
            drop(entry_writer);
            if destroyed {
                self.touch_key(key).await;
            }
            Ok(DataType::Integer(if destroyed { 1 } else { 0 }))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XGROUP CREATECONSUMER – Create a consumer in a group.
    pub async fn xgroup_createconsumer_cmd(
        &self,
        key: &str,
        group: &str,
        consumer: &str,
    ) -> Result<DataType, CacheError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Err(CacheError::InvalidCommandArguments(
                "ERR The XGROUP subcommand requires the key to exist. Note that for CREATE you may want to use the MKSTREAM option to create an empty stream automatically.".into(),
            ));
        };
        let entry = entry_writer.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            let result = match s.create_consumer(group, consumer, now_ms) {
                None => {
                    return Err(CacheError::InvalidCommandArguments(format!(
                        "NOGROUP No such key '{}' or consumer group '{}'",
                        key, group
                    )))
                }
                Some(b) => DataType::Integer(b as i64),
            };
            drop(entry_writer);
            self.touch_key(key).await;
            Ok(result)
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XGROUP DELCONSUMER – Remove a consumer from a group.
    pub async fn xgroup_delconsumer_cmd(
        &self,
        key: &str,
        group: &str,
        consumer: &str,
    ) -> Result<DataType, CacheError> {
        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Err(CacheError::InvalidCommandArguments(
                "ERR The XGROUP subcommand requires the key to exist. Note that for CREATE you may want to use the MKSTREAM option to create an empty stream automatically.".into(),
            ));
        };
        let entry = entry_writer.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            let result = match s.del_consumer(group, consumer) {
                None => {
                    return Err(CacheError::InvalidCommandArguments(format!(
                        "NOGROUP No such key '{}' or consumer group '{}'",
                        key, group
                    )))
                }
                Some(n) => DataType::Integer(n as i64),
            };
            drop(entry_writer);
            self.touch_key(key).await;
            Ok(result)
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XACK – Acknowledge processing of pending entries.
    ///
    /// Never errors on a missing key or group: those simply acknowledge nothing
    /// and return `0`. Malformed ids are an error.
    pub async fn xack_cmd(
        &self,
        key: &str,
        group: &str,
        ids: &[String],
    ) -> Result<DataType, CacheError> {
        let mut parsed_ids: Vec<StreamId> = Vec::with_capacity(ids.len());
        for id in ids {
            match stream::parse_id(id, 0) {
                Some(parsed) => parsed_ids.push(parsed),
                None => {
                    return Err(CacheError::InvalidCommandArguments(
                        "ERR Invalid stream ID specified as stream command argument".into(),
                    ))
                }
            }
        }

        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Ok(DataType::Integer(0));
        };
        let entry = entry_writer.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            if !s.group_exists(group) {
                return Ok(DataType::Integer(0));
            }
            let acked = s.ack(group, &parsed_ids).unwrap_or(0);
            drop(entry_writer);
            self.touch_key(key).await;
            Ok(DataType::Integer(acked as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XPENDING (summary form).
    pub async fn xpending_summary_cmd(
        &self,
        key: &str,
        group: &str,
    ) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Err(CacheError::InvalidCommandArguments(format!(
                "NOGROUP No such key '{}' or consumer group '{}'",
                key, group
            )));
        };
        let entry = entry_ref.value();

        if let DataType::Stream(s) = &entry.value {
            let sum = s.pending_summary(group).ok_or_else(|| {
                CacheError::InvalidCommandArguments(format!(
                    "NOGROUP No such key '{}' or consumer group '{}'",
                    key, group
                ))
            })?;

            let min = match sum.min_id {
                Some(id) => DataType::String(id.to_redis_string().into_bytes()),
                None => DataType::Nil,
            };
            let max = match sum.max_id {
                Some(id) => DataType::String(id.to_redis_string().into_bytes()),
                None => DataType::Nil,
            };
            let consumers = if sum.consumers.is_empty() {
                DataType::Nil
            } else {
                DataType::Array(
                    sum.consumers
                        .into_iter()
                        .map(|(name, count)| {
                            DataType::Array(vec![
                                DataType::String(name.into_bytes()),
                                DataType::String(count.to_string().into_bytes()),
                            ])
                        })
                        .collect(),
                )
            };

            Ok(DataType::Array(vec![
                DataType::Integer(sum.count as i64),
                min,
                max,
                consumers,
            ]))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XPENDING (extended form).
    #[allow(clippy::too_many_arguments)]
    pub async fn xpending_extended_cmd(
        &self,
        key: &str,
        group: &str,
        idle: Option<u64>,
        start: &str,
        end: &str,
        count: usize,
        consumer: Option<&str>,
    ) -> Result<DataType, CacheError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let start_id = stream::parse_range_start(start).ok_or_else(|| {
            CacheError::InvalidCommandArguments(
                "ERR Invalid stream ID specified as stream command argument".into(),
            )
        })?;
        let end_id = stream::parse_range_end(end).ok_or_else(|| {
            CacheError::InvalidCommandArguments(
                "ERR Invalid stream ID specified as stream command argument".into(),
            )
        })?;

        let Some(entry_ref) = self.data.get(key) else {
            return Err(CacheError::InvalidCommandArguments(format!(
                "NOGROUP No such key '{}' or consumer group '{}'",
                key, group
            )));
        };
        let entry = entry_ref.value();

        if let DataType::Stream(s) = &entry.value {
            let rows = s
                .pending_extended(group, start_id, end_id, count, consumer, idle, now_ms)
                .ok_or_else(|| {
                    CacheError::InvalidCommandArguments(format!(
                        "NOGROUP No such key '{}' or consumer group '{}'",
                        key, group
                    ))
                })?;

            let elems = rows
                .into_iter()
                .map(|row| {
                    DataType::Array(vec![
                        DataType::String(row.id.to_redis_string().into_bytes()),
                        DataType::String(row.consumer.into_bytes()),
                        DataType::Integer(row.idle_ms as i64),
                        DataType::Integer(row.delivery_count as i64),
                    ])
                })
                .collect();
            Ok(DataType::Array(elems))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XCLAIM – Change ownership of pending entries.
    #[allow(clippy::too_many_arguments)]
    pub async fn xclaim_cmd(
        &self,
        key: &str,
        group: &str,
        consumer: &str,
        min_idle: u64,
        ids: &[String],
        idle: Option<u64>,
        time: Option<u64>,
        retrycount: Option<u64>,
        force: bool,
        justid: bool,
    ) -> Result<DataType, CacheError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let mut parsed_ids: Vec<StreamId> = Vec::with_capacity(ids.len());
        for id in ids {
            match stream::parse_id(id, 0) {
                Some(parsed) => parsed_ids.push(parsed),
                None => {
                    return Err(CacheError::InvalidCommandArguments(
                        "ERR Invalid stream ID specified as stream command argument".into(),
                    ))
                }
            }
        }

        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Err(CacheError::InvalidCommandArguments(format!(
                "NOGROUP No such key '{}' or consumer group '{}'",
                key, group
            )));
        };
        let entry = entry_writer.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            let opt = ClaimOptions {
                min_idle_ms: min_idle,
                set_idle_ms: idle,
                set_time_ms: time,
                set_retry: retrycount,
                force,
                justid,
            };
            let claimed = s
                .claim(group, consumer, &parsed_ids, &opt, now_ms)
                .ok_or_else(|| {
                    CacheError::InvalidCommandArguments(format!(
                        "NOGROUP No such key '{}' or consumer group '{}'",
                        key, group
                    ))
                })?;
            drop(entry_writer);
            self.touch_key(key).await;
            if justid {
                Ok(DataType::Array(
                    claimed
                        .into_iter()
                        .map(|(id, _)| DataType::String(id.to_redis_string().into_bytes()))
                        .collect(),
                ))
            } else {
                Ok(Self::stream_entries_reply(claimed))
            }
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XAUTOCLAIM – Claim and return aged pending entries in bulk.
    #[allow(clippy::too_many_arguments)]
    pub async fn xautoclaim_cmd(
        &self,
        key: &str,
        group: &str,
        consumer: &str,
        min_idle: u64,
        start: &str,
        count: Option<usize>,
        justid: bool,
    ) -> Result<DataType, CacheError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let start_id = stream::parse_range_start(start).ok_or_else(|| {
            CacheError::InvalidCommandArguments(
                "ERR Invalid stream ID specified as stream command argument".into(),
            )
        })?;

        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Err(CacheError::InvalidCommandArguments(format!(
                "NOGROUP No such key '{}' or consumer group '{}'",
                key, group
            )));
        };
        let entry = entry_writer.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            let res = s
                .autoclaim(
                    group,
                    consumer,
                    min_idle,
                    start_id,
                    count.unwrap_or(100),
                    now_ms,
                    justid,
                )
                .ok_or_else(|| {
                    CacheError::InvalidCommandArguments(format!(
                        "NOGROUP No such key '{}' or consumer group '{}'",
                        key, group
                    ))
                })?;
            drop(entry_writer);
            self.touch_key(key).await;

            let claimed = if justid {
                DataType::Array(
                    res.claimed
                        .into_iter()
                        .map(|(id, _)| DataType::String(id.to_redis_string().into_bytes()))
                        .collect(),
                )
            } else {
                Self::stream_entries_reply(res.claimed)
            };
            let deleted = DataType::Array(
                res.deleted
                    .into_iter()
                    .map(|id| DataType::String(id.to_redis_string().into_bytes()))
                    .collect(),
            );

            Ok(DataType::Array(vec![
                DataType::String(res.cursor.to_redis_string().into_bytes()),
                claimed,
                deleted,
            ]))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XINFO STREAM – Summary of a stream's state.
    pub async fn xinfo_stream_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Err(CacheError::InvalidCommandArguments(
                "ERR no such key".into(),
            ));
        };
        let entry = entry_ref.value();

        if let DataType::Stream(s) = &entry.value {
            let entry_or_nil = |e: Option<stream::StreamEntry>| match e {
                Some(entry) => Self::stream_entry_element(entry),
                None => DataType::Nil,
            };
            let recorded_first = s.first_entry().map(|(id, _)| id).unwrap_or(StreamId::MIN);

            Ok(DataType::Array(vec![
                DataType::String(b"length".to_vec()),
                DataType::Integer(s.len() as i64),
                DataType::String(b"radix-tree-keys".to_vec()),
                DataType::Integer(1),
                DataType::String(b"radix-tree-nodes".to_vec()),
                DataType::Integer(2),
                DataType::String(b"last-generated-id".to_vec()),
                DataType::String(s.last_id().to_redis_string().into_bytes()),
                DataType::String(b"max-deleted-entry-id".to_vec()),
                DataType::String(s.max_deleted_id().to_redis_string().into_bytes()),
                DataType::String(b"entries-added".to_vec()),
                DataType::Integer(s.entries_added() as i64),
                DataType::String(b"recorded-first-entry-id".to_vec()),
                DataType::String(recorded_first.to_redis_string().into_bytes()),
                DataType::String(b"groups".to_vec()),
                DataType::Integer(s.groups_count() as i64),
                DataType::String(b"first-entry".to_vec()),
                entry_or_nil(s.first_entry()),
                DataType::String(b"last-entry".to_vec()),
                entry_or_nil(s.last_entry()),
            ]))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XINFO GROUPS – List consumer groups on a stream.
    pub async fn xinfo_groups_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let Some(entry_ref) = self.data.get(key) else {
            return Err(CacheError::InvalidCommandArguments(
                "ERR no such key".into(),
            ));
        };
        let entry = entry_ref.value();

        if let DataType::Stream(s) = &entry.value {
            let elems = s
                .group_infos()
                .into_iter()
                .map(|g| {
                    DataType::Array(vec![
                        DataType::String(b"name".to_vec()),
                        DataType::String(g.name.into_bytes()),
                        DataType::String(b"consumers".to_vec()),
                        DataType::Integer(g.consumers as i64),
                        DataType::String(b"pending".to_vec()),
                        DataType::Integer(g.pending as i64),
                        DataType::String(b"last-delivered-id".to_vec()),
                        DataType::String(g.last_delivered_id.to_redis_string().into_bytes()),
                        DataType::String(b"entries-read".to_vec()),
                        DataType::Integer(g.entries_read as i64),
                        DataType::String(b"lag".to_vec()),
                        DataType::Integer(g.lag as i64),
                    ])
                })
                .collect();
            Ok(DataType::Array(elems))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XINFO CONSUMERS – List consumers within a group.
    pub async fn xinfo_consumers_cmd(
        &self,
        key: &str,
        group: &str,
    ) -> Result<DataType, CacheError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let Some(entry_ref) = self.data.get(key) else {
            return Err(CacheError::InvalidCommandArguments(format!(
                "NOGROUP No such key '{}' or consumer group '{}'",
                key, group
            )));
        };
        let entry = entry_ref.value();

        if let DataType::Stream(s) = &entry.value {
            let cs = s.consumer_infos(group, now_ms).ok_or_else(|| {
                CacheError::InvalidCommandArguments(format!(
                    "NOGROUP No such key '{}' or consumer group '{}'",
                    key, group
                ))
            })?;

            let elems = cs
                .into_iter()
                .map(|c| {
                    DataType::Array(vec![
                        DataType::String(b"name".to_vec()),
                        DataType::String(c.name.into_bytes()),
                        DataType::String(b"pending".to_vec()),
                        DataType::Integer(c.pending as i64),
                        DataType::String(b"idle".to_vec()),
                        DataType::Integer(c.idle_ms as i64),
                        DataType::String(b"inactive".to_vec()),
                        DataType::Integer(c.inactive_ms as i64),
                    ])
                })
                .collect();
            Ok(DataType::Array(elems))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Command: XSETID – Set a stream's last-generated id and counters.
    pub async fn xsetid_cmd(
        &self,
        key: &str,
        id: &str,
        entries_added: Option<u64>,
        max_deleted: Option<&str>,
    ) -> Result<DataType, CacheError> {
        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Err(CacheError::InvalidCommandArguments(
                "ERR no such key".into(),
            ));
        };
        let entry = entry_writer.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            let resolved = if id == "$" {
                s.last_id()
            } else {
                stream::parse_id(id, 0).ok_or_else(|| {
                    CacheError::InvalidCommandArguments(
                        "ERR Invalid stream ID specified as stream command argument".into(),
                    )
                })?
            };

            if let Some(top) = s.max_entry_id() {
                if resolved < top {
                    return Err(CacheError::InvalidCommandArguments(
                        "ERR The ID specified in XSETID is smaller than the target stream top item"
                            .into(),
                    ));
                }
            }

            let max_deleted_parsed = match max_deleted {
                Some(tok) => Some(stream::parse_id(tok, 0).ok_or_else(|| {
                    CacheError::InvalidCommandArguments(
                        "ERR Invalid stream ID specified as stream command argument".into(),
                    )
                })?),
                None => None,
            };

            s.set_last_id(resolved, entries_added, max_deleted_parsed);
            drop(entry_writer);
            self.touch_key(key).await;
            Ok(DataType::Status("OK".into()))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "stream".to_string(),
                entry.value.data_type(),
            ))
        }
    }

    /// Read one stream element for a multi-key `XREADGROUP`.
    ///
    /// A missing key, non-stream value, or absent group is a `NOGROUP` error.
    /// For the `>` token an empty result is omitted (`Ok(None)`); explicit-id
    /// (history) reads always produce an element.
    #[allow(clippy::too_many_arguments)]
    pub async fn xreadgroup_one(
        &self,
        key: &str,
        group: &str,
        consumer: &str,
        id_token: &str,
        count: Option<usize>,
        noack: bool,
        now_ms: u64,
    ) -> Result<Option<DataType>, CacheError> {
        let Some(mut entry_writer) = self.data.get_mut(key) else {
            return Err(CacheError::InvalidCommandArguments(format!(
                "NOGROUP No such key '{}' or consumer group '{}'",
                key, group
            )));
        };
        let entry = entry_writer.value_mut();

        if let DataType::Stream(s) = &mut entry.value {
            if !s.group_exists(group) {
                return Err(CacheError::InvalidCommandArguments(format!(
                    "NOGROUP No such key '{}' or consumer group '{}'",
                    key, group
                )));
            }

            if id_token == ">" {
                let entries = s
                    .group_read_new(group, consumer, count, now_ms, noack)
                    .unwrap();
                drop(entry_writer);
                self.touch_key(key).await;
                if entries.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(DataType::Array(vec![
                        DataType::String(key.to_string().into_bytes()),
                        Self::stream_entries_reply(entries),
                    ])))
                }
            } else {
                let from = if id_token == "0" || id_token == "0-0" {
                    StreamId::MIN
                } else {
                    stream::parse_id(id_token, 0).ok_or_else(|| {
                        CacheError::InvalidCommandArguments(
                            "ERR Invalid stream ID specified as stream command argument".into(),
                        )
                    })?
                };
                let entries = s
                    .group_read_history(group, consumer, from, count, now_ms)
                    .unwrap();
                drop(entry_writer);
                self.touch_key(key).await;
                Ok(Some(DataType::Array(vec![
                    DataType::String(key.to_string().into_bytes()),
                    Self::stream_entries_reply(entries),
                ])))
            }
        } else {
            Err(CacheError::InvalidCommandArguments(format!(
                "NOGROUP No such key '{}' or consumer group '{}'",
                key, group
            )))
        }
    }
}

impl CalodShard {
    /// Persist this shard's in-memory dataset to a binary snapshot at `path`.
    pub async fn save(&self, path: &str) -> Result<(), PersistenceError> {
        let data = self
            .data
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect::<Vec<_>>();

        let encoded = serialize(&data).map_err(PersistenceError::Serialization)?;

        write(path, &encoded).await.map_err(PersistenceError::Io)?;

        Ok(())
    }

    /// Load a snapshot from `path`, replacing (merging over) existing keys.
    /// Duplicate keys in the snapshot overwrite current values.
    pub async fn load(&self, path: &str) -> Result<(), PersistenceError> {
        let encoded = read(path).await.map_err(PersistenceError::Io)?;
        let data: Vec<(String, CacheEntry)> =
            deserialize(&encoded).map_err(PersistenceError::Serialization)?;

        self.data.clear();
        self.size.store(0, Ordering::Relaxed);
        self.metrics.total_data_size.store(0, Ordering::Relaxed);

        for (key, entry) in data {
            self.metrics
                .total_data_size
                .fetch_add(entry.size() as u64, Ordering::Relaxed);
            self.data.insert(key, entry);
            self.size.fetch_add(1, Ordering::Relaxed);
        }

        Ok(())
    }
}
