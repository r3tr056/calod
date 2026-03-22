use dashmap::DashMap;
use lru::LruCache;
use tokio::sync::broadcast;
use tokio::time::timeout;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{Ordering, AtomicUsize};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{error, trace};
use serde_json::{json, Error as SerdeJsonError, Value as JsonValue};

use bincode::{serialize, deserialize};
use tokio::fs::{read, write};
use crate::cld_srv::command::{InsertOption, SetExpireOption, SetOption};
use crate::store::calod_data::{CacheEntry, EvictionCandidate};
use crate::extensions::fastgraphdb::base::{Edge, GraphData, Node};

use super::calod_data::DataType;
use super::error::{CacheError, PersistenceError};
use super::helpers::json_helpers::{json_type_to_string, jsonpath_arrappend, jsonpath_arrindex, jsonpath_arrinsert, jsonpath_arrlen, jsonpath_arrpop, jsonpath_arrtrim, jsonpath_del, jsonpath_get, jsonpath_numincrby, jsonpath_objkeys, jsonpath_objlen, jsonpath_objset, jsonpath_set, jsonpath_strappend};
use super::metrics::CalodMetrics;

static OK_RESPONSE: &str = "OK";
static NONE_TYPE: &str = "none";

/// A single shard of the Calod in‑memory data store.
///
/// Each `CalodShard` manages a partition of the global keyspace. It provides:
///
/// * Concurrent access to heterogeneous value types (`DataType`).
/// * Per‑entry TTL / expiration management.
/// * An approximate LRU based eviction policy backed by an in‑memory `LruCache` of key hashes.
/// * Pub/Sub style list modification notifications used for blocking list commands (BLPOP / BRPOP).
/// * JSON (document) sub‑path manipulation helpers.
/// * Hash / List / String / Numeric / Graph primitives inspired by Redis semantics.
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
        let lru_capacity = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(1000).unwrap());
        let lru_cache = Arc::new(RwLock::new(LruCache::new(lru_capacity)));

        Self {
            data,
            lru_cache,
            capacity: AtomicUsize::new(capacity),
            size: AtomicUsize::new(0),
            list_modification_notifier: broadcast::channel(1024).0,
            metrics,
            id
        }
    }

    /// Return this shard's numeric identifier.
    pub fn id(&self) -> usize { self.id }

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
            Ok(DataType::String(entry.value.data_type()))
        } else {
            Ok(DataType::String(NONE_TYPE.to_string()))
        }
    }

    /// Command: KEYS – Return keys containing the provided substring `pattern`.
    /// NOTE: This is a simple substring match, not a glob. Expired keys are skipped.
    pub async fn keys(&self, pattern: &str) -> Vec<String> {
        trace!("Shard getting keys with pattern: {}", pattern);

        let estimated_size = (self.size.load(Ordering::Relaxed) / 10).max(16);
        let mut results = Vec::with_capacity(estimated_size);

        self.data.iter().for_each(|entry| {
            if entry.key().contains(pattern) && !entry.value().is_expired() {
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
            entry_writer.value_mut().expire_in(Duration::from_secs(seconds));
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
                drop(entry_ref);        // Drop read guard before deletion
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
            self.metrics.total_data_size.fetch_sub(entry.size() as u64, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            true
        } else {
            false
        };
        
        result
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
    pub async fn set_cmd(&self, key: String, value: DataType, expire_option: Option<SetExpireOption>, set_option: Option<SetOption>) -> Result<(), CacheError> {
        let ttl_duration: Option<Duration> = match expire_option {
            Some(SetExpireOption::EX(seconds)) => Some(Duration::from_secs(seconds)),
            Some(SetExpireOption::PX(milliseconds)) => Some(Duration::from_millis(milliseconds)),
            None => None,
        };

        // Check NX/XX conditions first to avoid unnecessary work
        if let Some(option) = set_option {
            match option {
                SetOption::NX => { if self.exists(&key).await { return Ok(()); } },
                SetOption::XX => { if !self.exists(&key).await { return Ok(()); } }
            }
        }

        self.set(key, value, ttl_duration).await;
        Ok(())
    }

    /// Internal helper implementing unconditional set with optional TTL.
    #[inline]
    pub async fn set(&self, key: String, value: DataType, ttl: Option<Duration>) -> Option<DataType> {
        trace!("Shard setting key: {}", key);
        
        // check if we need to evict before adding a new entry
        if self.size.load(Ordering::Relaxed) >= self.capacity.load(Ordering::Relaxed) {
            self.evict().await;
        }

        let entry = CacheEntry::new(value, ttl);
        let entry_size = entry.size();
        let old_entry = if let Some(old_e) = self.data.insert(key.clone(), entry) {
            self.metrics.total_data_size.fetch_sub(old_e.size() as u64, Ordering::Relaxed);
            Some(old_e.value)
        } else {
            self.size.fetch_add(1, Ordering::Relaxed);
            None
        };

        // update size metrics
        self.metrics.total_data_size.fetch_add(entry_size as u64, Ordering::Relaxed);
        self.touch_key(&key).await;
        old_entry
    }

    /// Command: APPEND – Append bytes to existing string or create a new one. Returns new length.
    #[inline]
    pub async fn append_cmd(&self, key: &str, value: String) -> Result<DataType, CacheError> {
        // Fast path for common case - check if key exists and is string
        if let Some(mut entry_writer) = self.data.get_mut(key) {
            let entry = entry_writer.value_mut();
            
            // Handle string case
            if let DataType::String(s) = &mut entry.value {
                s.push_str(&value);
                let new_len = s.len();
                self.touch_key(key).await;
                return Ok(DataType::Integer(new_len as i64));
            } else {
                return Err(CacheError::DataTypeMismatch(
                    key.to_string(), 
                    "String".to_string(), 
                    entry.value.data_type()
                ));
            }
        }
        
        // Key doesn't exist, create new entry with pre-allocated capacity
        let len = value.len();
        self.set(key.to_string(), DataType::String(value), None).await;
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
                },
                DataType::Nil => Ok(DataType::Integer(0)),
                _ => Err(CacheError::DataTypeMismatch(
                    key.to_string(), 
                    "string".to_string(), 
                    entry.value.data_type()
                )),
            }
        } else {
            Ok(DataType::Integer(0))
        }
    }

    /// Command: GETRANGE – Return substring (inclusive end index logic consistent with Redis).
    pub async fn getrange_cmd(&self, key: &str, start: isize, end: isize) -> Result<DataType, CacheError> {
        if let Some(entry_ref) = self.data.get(key) {
            let entry = entry_ref.value();

            if entry.is_expired() {
                drop(entry_ref);
                self.data.remove(key);
                self.size.fetch_sub(1, Ordering::Relaxed);
                self.remove_from_lru(key).await;
                return Ok(DataType::String("".to_string()));
            }

            if let DataType::String(s) = &entry.value {
                let len = s.len() as isize;

                let start_index = if start < 0 { (start + len).max(0) } else { start.min(len) } as usize;
                let end_index = if end < 0 { (end + len).max(0) } else { end.min(len) } as usize;

                if start_index > end_index || start_index >= s.len() {
                    return Ok(DataType::String("".to_string()))
                }

                // Use efficient substring operation
                let result = if end_index >= s.len() {
                    s[start_index..].to_owned()
                } else {
                    s[start_index..=end_index].to_owned()
                };
                
                self.touch_key(key).await;
                Ok(DataType::String(result))
            } else {
                Err(CacheError::DataTypeMismatch(
                    key.to_string(), 
                    "string".to_string(), 
                    entry.value.data_type()
                ))
            }
        } else {
            Ok(DataType::String("".to_string()))
        }
    }

    /// Command: SETRANGE – Overwrite string starting at `offset`, padding with null bytes if required.
    #[inline]
    pub async fn setrange_cmd(&self, key: &str, offset: usize, value: String) -> Result<DataType, CacheError> {
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
                // Create new empty string if key does not exist
                let new_string = if offset > 0 {
                    // Pre-allocate with zeros if offset > 0
                    let mut s = String::with_capacity(offset + value.len());
                    s.extend(std::iter::repeat('\0').take(offset));
                    s
                } else {
                    String::new()
                };
                
                self.set(key.to_string(), DataType::String(new_string), None).await;
                self.data.get_mut(key).unwrap() // Safe since we just inserted it
            }
        };
        
        let entry = entry_writer.value_mut();
        
        if let DataType::String(s) = &mut entry.value {
            // Ensure the string is long enough
            if offset > s.len() {
                // Add null bytes as padding
                s.extend(std::iter::repeat('\0').take(offset - s.len()));
            }
            
            // Efficient string modification
            if offset == s.len() {
                // Append at end (common case)
                s.push_str(&value);
            } else {
                // Replace in middle
                let prefix = if offset > 0 { &s[..offset] } else { "" };
                let new_value = format!("{}{}{}", 
                    prefix,
                    value,
                    if offset + value.len() < s.len() { &s[offset + value.len()..] } else { "" }
                );
                *s = new_value;
            }
            
            self.touch_key(key).await;
            Ok(DataType::Integer(s.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "String".to_string(),
                entry.value.data_type()
            ))
        }
    }

    /// Command: GETSET – Atomically get previous value and set to new string.
    #[inline]
    pub async fn getset_cmd(&self, key: &str, value: String) -> Result<DataType, CacheError> {
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

        self.set(key.to_string(), DataType::String(value), None).await;
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
                    return Ok(DataType::Integer(*n))
                }
                DataType::String(s) => {
                    match s.parse::<i64>() {
                        Ok(mut n) => {
                            n += increment;
                            entry.value = DataType::Integer(n);
                            self.touch_key(key).await;
                            return Ok(DataType::Integer(n))
                        }
                        Err(_) => return Err(CacheError::NotAnInteger)
                    }
                }
                DataType::Nil => { // Treat Nil as 0 for INCRBY
                    entry.value = DataType::Integer(increment);
                    self.touch_key(key).await;
                    return Ok(DataType::Integer(increment))
                }
                _ => return Err(CacheError::DataTypeMismatch(
                    key.to_string(),
                    "Integer or String representable as integer".to_string(),
                    entry.value.data_type()
                )),
            }
        }

        self.set(key.to_string(), DataType::Integer(increment), None).await;
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
                DataType::String(s) => {
                    match s.parse::<f64>() {
                        Ok(n) => n + increment,
                        Err(_) => return Err(CacheError::InvalidScoreFormat)
                    }
                }
                DataType::Nil => increment,
                _ => return Err(CacheError::DataTypeMismatch(key.to_string(), "String representable as float".to_string(), entry.value.data_type())),
            };

            let result_str = format!("{}", result);
            entry.value = DataType::String(result_str.clone());
            self.touch_key(key).await;

            Ok(DataType::String(result_str))
        } else {
            let result_str = format!("{}", increment);
            self.set(key.to_string(), DataType::String(result_str.clone()), None).await;
            Ok(DataType::String(result_str))
        }
    }

    // Hash Commands
    /// Command: HSET – Set one or more hash fields. Returns number of new fields added.
    pub async fn hset_cmd(&self, key: &str, field_values: Vec<(String, String)>) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.to_string()).or_insert_with(|| CacheEntry::new(DataType::Hash(DashMap::new()), None));
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
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

    /// Command: HGET – Get a single hash field.
    pub async fn hget_cmd(&self, key: &str, field: String) -> Result<DataType, CacheError> {
        let entry_ref = self.data.get(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_ref.value();

        if let DataType::Hash(hash_map) = &entry.value {
            match hash_map.get(&field) {
                Some(value) => {
                    self.touch_key(key).await;
                    Ok(DataType::String(value.clone()))
                },
                None => Ok(DataType::Nil), // Field not found returns Nil Bulk String
            }
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

    /// Command: HDEL – Delete one or more hash fields. Returns count removed.
    pub async fn hdel_cmd(&self, key: &str, fields: Vec<String>) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();
        let mut count_deleted_fields = 0;

        if let DataType::Hash(hash_map) = &mut entry.value {
            for field in fields {
                if hash_map.remove(&field).is_some() {
                    count_deleted_fields += 1;
                }
            }
            self.touch_key(key).await;
            Ok(DataType::Integer(count_deleted_fields)) // Return number of deleted fields
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

    /// Command: HEXISTS – Test hash field existence (1 / 0).
    pub async fn hexists_cmd(&self, key: &str, field: String) -> Result<DataType, CacheError> {
        let entry_ref = self.data.get(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_ref.value();

        if let DataType::Hash(hash_map) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Integer(if hash_map.contains_key(&field) { 1 } else { 0 })) // 1 if field exists, 0 if not
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

    /// Command: HGETALL – Return full hash snapshot.
    pub async fn hgetall_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let entry_ref = self.data.get(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_ref.value();

        if let DataType::Hash(hash_map) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Hash(hash_map.clone())) // Return a clone to avoid borrowing issues
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

    /// Command: HINCRBY – Increment integer stored at hash field.
    pub async fn hincrby_cmd(&self, key: &str, field: String, increment: i64) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.to_string()).or_insert_with(|| CacheEntry::new(DataType::Hash(DashMap::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::Hash(hash_map) = &mut entry.value {
            let mut current_value: i64 = 0;
            if let Some(val_str) = hash_map.get(&field) {
                current_value = val_str.parse::<i64>().map_err(|_| CacheError::NotAnInteger)?;
            }
            current_value += increment;
            hash_map.insert(field, current_value.to_string());
            self.touch_key(key).await;
            Ok(DataType::Integer(current_value))
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

    /// Command: HINCRBYFLOAT – Increment float stored at hash fields (batched variant returning updated map subset).
    pub async fn hincrbyfloat_cmd(&self, key: &str, key_field_increment: Vec<(String, f64)>) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.to_string()).or_insert_with(|| CacheEntry::new(DataType::Hash(DashMap::new()), None));
        let entry = entry_mut.value_mut();
        let response_map = DashMap::new();

        if let DataType::Hash(hash_map) = &mut entry.value {
            for (field, increment) in key_field_increment {
                let mut current_value: f64 = 0.0;
                if let Some(val_str) = hash_map.get(&field) {
                    current_value = val_str.parse::<f64>().map_err(|_| CacheError::InvalidScoreFormat)?;
                }
                current_value += increment;
                hash_map.insert(field.clone(), current_value.to_string());
                response_map.insert(field, current_value.to_string()); // Store for response
            }
            self.touch_key(key).await;
            Ok(DataType::Hash(response_map)) // Returning a Hash DataType with all updated fields and values
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }


    /// Command: HKEYS – Return vector of field names.
    pub async fn hkeys_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let entry_ref = self.data.get(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_ref.value();

        if let DataType::Hash(hash_map) = &entry.value {
            self.touch_key(key).await;
            let keys: Vec<String> = hash_map.iter().map(|entry| entry.key().clone()).collect();
            Ok(DataType::List(keys.into()))
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

    /// Command: HLEN – Return number of fields.
    pub async fn hlen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let entry_ref = self.data.get(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_ref.value();

        if let DataType::Hash(hash_map) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Integer(hash_map.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

    /// Command: HMGET – Bulk get of multiple hash fields.
    pub async fn hmget_cmd(&self, key: &str, fields: Vec<String>) -> Result<DataType, CacheError> {
        let entry_ref = self.data.get(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_ref.value();
        let mut results: Vec<String> = Vec::with_capacity(fields.len());

        if let DataType::Hash(hash_map) = &entry.value {
            for field in fields {
                match hash_map.get(&field) {
                    Some(value) => results.push(value.clone()),
                    None => results.push("".to_string()),
                }
            }
            self.touch_key(key).await;
            Ok(DataType::List(results.into()))
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

    /// Command: HMSET – Bulk set of multiple hash fields.
    pub async fn hmset_cmd(&self, key: &str, field_values: Vec<(String, String)>) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.to_string()).or_insert_with(|| CacheEntry::new(DataType::Hash(DashMap::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::Hash(hash_map) = &mut entry.value {
            for (field, value) in field_values {
                hash_map.insert(field, value);
            }
            self.touch_key(key).await;
            Ok(DataType::String(OK_RESPONSE.to_string())) // HMSET returns OK Simple String
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

    /// Command: HSETNX – Set hash field only if absent.
    pub async fn hsetnx_cmd(&self, key: &str, field: String, value: String) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.to_string()).or_insert_with(|| CacheEntry::new(DataType::Hash(DashMap::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::Hash(hash_map) = &mut entry.value {
            if !hash_map.contains_key(&field) {
                hash_map.insert(field, value);
                self.touch_key(key).await;
                Ok(DataType::Integer(1)) // 1 if field was set
            } else {
                Ok(DataType::Integer(0)) // 0 if field was not set (already exists)
            }
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

    /// Command: HVALS – Return collection of hash values.
    pub async fn hvals_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let entry_ref = self.data.get(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_ref.value();

        if let DataType::Hash(hash_map) = &entry.value {
            self.touch_key(key).await;
            let values: Vec<String> = hash_map.iter().map(|entry| entry.value().clone()).collect();
            Ok(DataType::List(values.into()))
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

    // List commands

    /// Command: LPUSH – Push values to the head of list (reverse order of arguments).
    pub async fn lpush_cmd(&self, key: String, values: Vec<String>) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.clone()).or_insert_with(|| CacheEntry::new(DataType::List(Vec::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::List(list) = &mut entry.value {
            for value in values.into_iter().rev() { // LPUSH inserts values in reverse order of arguments
                list.insert(0, value);
            }
            self.touch_key(&key).await;
            let _ = self.list_modification_notifier.send(key.clone());
            Ok(DataType::Integer(list.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(key, "List".to_string(), entry.value.data_type()))
        }
    }

    /// Command: RPUSH – Push values to the tail of list.
    pub async fn rpush_cmd(&self, key: String, values: Vec<String>) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.clone()).or_insert_with(|| CacheEntry::new(DataType::List(Vec::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::List(list) = &mut entry.value {
            for value in values {
                list.push(value);
            }
            self.touch_key(&key).await;
            let _ = self.list_modification_notifier.send(key.clone());
            Ok(DataType::Integer(list.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(key, "List".to_string(), entry.value.data_type()))
        }
    }

    /// Command: LPOP – Pop head element.
    pub async fn lpop_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::List(list) = &mut entry.value {
            if list.is_empty() {
                Ok(DataType::Nil)
            } else {
                let value = list.remove(0);
                self.touch_key(key).await;
                Ok(DataType::String(value))
            }
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "List".to_string(), entry.value.data_type()))
        }
    }

    /// Command: RPOP – Pop tail element.
    pub async fn rpop_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::List(list) = &mut entry.value {
            if list.is_empty() {
                Ok(DataType::Nil)
            } else {
                let value = list.pop().unwrap(); // Safe because we checked for emptiness
                self.touch_key(key).await;
                Ok(DataType::String(value))
            }
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "List".to_string(), entry.value.data_type()))
        }
    }

    /// Command: LLEN – List length.
    pub async fn llen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let entry_ref = self.data.get(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_ref.value();

        if let DataType::List(list) = &entry.value {
            self.touch_key(key).await;
            Ok(DataType::Integer(list.len() as i64))
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "List".to_string(), entry.value.data_type()))
        }
    }

    /// Command: LRANGE – Return sub-slice (inclusive end) of list.
    pub async fn lrange_cmd(&self, key: &str, start: isize, end: isize) -> Result<DataType, CacheError> {
        let entry_ref = self.data.get(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_ref.value();

        if let DataType::List(list) = &entry.value {
            let len = list.len() as isize;
            let start_index = if start < 0 { (start + len).max(0) } else { start }.min(len);
            let end_index = if end < 0 { (end + len).max(0) } else { end }.min(len);

            if start_index > end_index {
                return Ok(DataType::List(Vec::new())); // Empty range
            }

            let range = list[start_index as usize..end_index as usize + 1].to_vec();
            self.touch_key(key).await;
            Ok(DataType::List(range.into())) // Convert Vec<String> to LinkedList<String>
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "List".to_string(), entry.value.data_type()))
        }
    }

    /// Command: LINDEX – Random access by positive / negative index.
    pub async fn lindex_cmd(&self, key: &str, index: isize) -> Result<DataType, CacheError> {
        let entry_ref = self.data.get(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_ref.value();

        if let DataType::List(list) = &entry.value {
            let len = list.len() as isize;
            let index_pos = if index < 0 { index + len } else { index };
            if index_pos < 0 || index_pos >= len {
                return Ok(DataType::Nil); // Index out of range returns Nil Bulk String
            }
            self.touch_key(key).await;
            Ok(DataType::String(list[index_pos as usize].clone()))
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "List".to_string(), entry.value.data_type()))
        }
    }

    /// Command: LINSERT – Insert before / after pivot.
    pub async fn linsert_cmd(&self, key: &str, before_after: InsertOption, pivot: String, value: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
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
            Err(CacheError::DataTypeMismatch(key.to_string(), "List".to_string(), entry.value.data_type()))
        }
    }

    /// Command: LSET – Overwrite element at index.
    pub async fn lset_cmd(&self, key: &str, index: isize, value: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::List(list) = &mut entry.value {
            let len = list.len() as isize;
            let index_pos = if index < 0 { index + len } else { index };

            if index_pos < 0 || index_pos >= len {
                return Err(CacheError::IndexOutOfRange); // Index out of range returns error
            }
            list[index_pos as usize] = value;
            self.touch_key(key).await;
            Ok(DataType::String(OK_RESPONSE.to_string())) // LSET returns OK Simple String
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "List".to_string(), entry.value.data_type()))
        }
    }

    /// Command: LTRIM – Keep only specified inclusive range.
    pub async fn ltrim_cmd(&self, key: &str, start: isize, end: isize) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::List(list) = &mut entry.value {
            let len = list.len() as isize;
            let start_index = if start < 0 { (start + len).max(0) } else { start }.min(len);
            let end_index = if end < 0 { (end + len).max(0) } else { end }.min(len);

            if start_index > end_index {
                list.clear(); // Trim to empty list if range is invalid
            } else {
                let trimmed_list: Vec<String> = list[start_index as usize..=end_index as usize].to_vec();
                entry.value = DataType::List(trimmed_list.into()); // Replace list with trimmed version
            }
            self.touch_key(key).await;
            Ok(DataType::String(OK_RESPONSE.to_string())) // LTRIM returns OK Simple String
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "List".to_string(), entry.value.data_type()))
        }
    }
    
    /// Command: LREM – Remove occurrences of value according to count semantics.
    pub async fn lrem_cmd(&self, key: &str, count: i64, value: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
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
            Err(CacheError::DataTypeMismatch(key.to_string(), "List".to_string(), entry.value.data_type()))
        }
    }

    fn lrem_positive_count(list: &mut Vec<String>, count: usize, value: &str) -> usize {
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

    fn lrem_negative_count(list: &mut Vec<String>, count: usize, value: &str) -> usize {
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

    fn lrem_zero_count(list: &mut Vec<String>, value: &str) -> usize {
        let initial_len = list.len();
        list.retain(|x| x != value);
        initial_len - list.len() // Count of removed elements
    }


    /// Command: RPOPLPUSH – Pop from source tail and push to destination head.
    pub async fn rpoplpush_cmd(&self, source: &str, destination: &str) -> Result<DataType, CacheError> {
        let popped_value_result = self.rpop_cmd(source).await; // RPOP from source list

        match popped_value_result {
            Ok(DataType::String(popped_value)) => {
                self.lpush_cmd(destination.to_string(), vec![popped_value.clone()]).await?; // LPUSH to destination
                Ok(DataType::String(popped_value)) // Return popped value
            },
            Ok(DataType::Nil) => Ok(DataType::Nil), // Source list empty, return Nil Bulk String
            Ok(_) => Err(CacheError::DataTypeMismatch(source.to_string(), "List".to_string(), "Non-List".to_string())), // Should not happen if rpop_cmd is correctly implemented for lists
            Err(e) => Err(e), // Propagate errors from rpop_cmd
        }
    }

    /// Command: BLPOP – Blocking pop (head) from first non-empty list among `keys` within timeout.
    pub async fn blpop_cmd(&self, keys: Vec<String>, timeout_sec: f64) -> Result<DataType, CacheError> {
        let timeout_duration = Duration::from_secs_f64(timeout_sec.max(0.0));
        let mut notification_receiver = self.list_modification_notifier.subscribe();

        // initial non-blocking call
        for key in &keys {
            if let Ok(DataType::String(value)) = self.lpop_cmd(key).await {
                if value != "nil" {
                    return Ok(DataType::List(Vec::<String>::from([key.clone(), value])));
                }
            }
        }
        
        let start_time = tokio::time::Instant::now();
        loop {
            let remaining_timeout = timeout_duration.saturating_sub(start_time.elapsed());
            if remaining_timeout.is_zero() {
                return Ok(DataType::Nil)
            }

            match timeout(remaining_timeout, notification_receiver.recv()).await {
                Ok(Ok(notified_key)) => {
                    if keys.contains(&notified_key) {
                        if let Ok(DataType::String(value)) = self.lpop_cmd(&notified_key).await {
                            if value != "nil" {
                                return Ok(DataType::List(Vec::<String>::from([notified_key, value])));
                            }
                        }
                    }
                },
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    error!("List modification notification channel closed unexpectedly.");
                    return Err(CacheError::InternalError("List notification channel closed".to_string()));
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) | Err(_)=> {
                    return Ok(DataType::Nil);
                },
            }
        }
    }

    /// Command: BRPOP – Blocking pop (tail) variant.
    pub async fn brpop_cmd(&self, keys: Vec<String>, timeout_sec: f64) -> Result<DataType, CacheError> {
        let timeout_duration = Duration::from_secs_f64(timeout_sec.max(0.0));
        let mut notification_receiver = self.list_modification_notifier.subscribe();

        for key in &keys {
            if let Ok(DataType::String(value)) = self.rpop_cmd(key).await {
                if value != "nil" { 
                    return Ok(DataType::List(Vec::<String>::from([key.clone(), value])));
                }
            }
        }

        // 2. Blocking wait with timeout
        let start_time = tokio::time::Instant::now();
        loop {
            let remaining_timeout = timeout_duration.saturating_sub(start_time.elapsed());
            if remaining_timeout.is_zero() {
                return Ok(DataType::Nil);
            }

            match timeout(remaining_timeout, notification_receiver.recv()).await {
                Ok(Ok(notified_key)) => {
                    if keys.contains(&notified_key) {
                        if let Ok(DataType::String(value)) = self.rpop_cmd(&notified_key).await {
                            if value != "nil" {
                                return Ok(DataType::List(Vec::<String>::from([notified_key, value])));
                            }
                        }
                    }
                },
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    error!("List modification notification channel closed unexpectedly.");
                    return Err(CacheError::InternalError("List notification channel closed".to_string()));
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) | Err(_)=> {
                    return Ok(DataType::Nil);
                },
            }
        }
    }

    // Vectorized batch operations for high throughput
    #[cfg(target_arch="x86_64")]
    /// Experimental: vectorized multi-get using rayon parallel chunks (x86_64 only).
    #[cfg(target_arch="x86_64")]
    pub async fn mget_vectorized(&self, keys: &[String]) -> Vec<Option<DataType>> {
        use rayon::{iter::ParallelIterator, slice::ParallelSlice};

        const BATCH_SIZE: usize = 8;
        let mut results = Vec::with_capacity(keys.len());

        // Process in SIMD-friendly batches
        keys.par_chunks(BATCH_SIZE).map(|chunk| {
            chunk.iter().map(|key| {
                self.data.get(key).map(|entry| {
                    if entry.is_expired() { None } else { Some(entry.value.clone()) }
                }).flatten()
            }).collect::<Vec<_>>()
        }).collect::<Vec<_>>()
            .into_iter()
            .for_each(|batch| results.extend(batch));

        results
    }

    /// Needle in a haystack problem - SIMD string comparison for patttern matching
    #[cfg(target_arch="x86_64")]
    unsafe fn simd_string_contains(haystack: &str, needle: &str) -> bool {
        use std::arch::x86_64::{__m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8};

        if needle.len() > haystack.len() || needle.is_empty() {
            return false;
        }

        let haystack_bytes = haystack.as_bytes();
        let needle_bytes = needle.as_bytes();

        if needle.len() >= 16 {
            // Use AVX2 for longer patterns
            let needle_chunk = _mm256_loadu_si256(needle_bytes.as_ptr() as *const __m256i);
            for i in 0..=(haystack_bytes.len() - 16) {
                let haystack_chunk = _mm256_loadu_si256(haystack_bytes[i..].as_ptr() as *const __m256i);
                let cmp = _mm256_cmpeq_epi8(needle_chunk, haystack_chunk);
                if _mm256_movemask_epi8(cmp) != 0 {
                    return true;
                }
            }
        }

        // Fallback to standard search
        haystack.contains(needle)
    }

    /// Vecotrized keys pattern matching
    /// Vectorized KEYS optimization (uses SIMD substring when available); best-effort.
    pub async fn keys_vectorized(&self, pattern: &str) -> Vec<String> {
        self.data.iter().filter_map(|entry| {
            let key = entry.key();
            if !entry.value().is_expired() {
                #[cfg(target_arch="x86_64")]
                unsafe {
                    if Self::simd_string_contains(key, pattern) {
                        return Some(key.clone());
                    }
                }
                #[cfg(not(target_arch="x86_64"))]
                if key.contains(pattern) {
                    return Some(key.clone());
                }
            }
            None
        }).collect()
    }

    // --- JSON Command Handlers ---
    /// Command: JSON.SET – Set JSON value at JSONPath.
    pub async fn json_set_cmd(&self, key: String, path: String, value_str: String) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.clone()).or_insert_with(|| CacheEntry::new(DataType::Document(JsonValue::Null), None));
        let entry = entry_mut.value_mut();

        let json_value: JsonValue = serde_json::from_str(&value_str).map_err(|e: SerdeJsonError| CacheError::InvalidCommandArguments(format!("Invalid JSON value: {}", e)))?;

        if let DataType::Document(doc) = &mut entry.value {
            let result_json = jsonpath_set(doc, &path, json_value)?;
            entry.value = DataType::Document(result_json);
            self.touch_key(&key).await;
            Ok(DataType::String(OK_RESPONSE.to_string()))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Document".to_string(), entry.value.data_type()))
        }
    }

    /// Command: JSON.GET – Get JSON sub-value by path.
    pub async fn json_get_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let result_json_value = jsonpath_get(&doc, &path)?;
                Ok(DataType::Document(result_json_value))
            },
            DataType::Nil => Ok(DataType::Nil), // Key not found returns Nil
            other => Err(CacheError::DataTypeMismatch(key, "Document".to_string(), other.data_type())),
        }
    }

    /// Command: JSON.DEL – Delete JSON path returning number removed.
    pub async fn json_del_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Document(doc) = &mut entry.value {
            let deleted_count = jsonpath_del(doc, &path)?;
            self.touch_key(&key).await;
            Ok(DataType::Integer(deleted_count as i64))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Document".to_string(), entry.value.data_type()))
        }
    }

    /// Command: JSON.TYPE – Return type string for JSON path.
    pub async fn json_type_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let json_value = jsonpath_get(&doc, &path)?;
                Ok(DataType::String(json_type_to_string(&json_value)))
            },
            DataType::Nil => Ok(DataType::Nil), // Key not found returns Nil
            other => Err(CacheError::DataTypeMismatch(key, "Document".to_string(), other.data_type())),
        }
    }

    /// Command: JSON.NUMINCRBY – Numeric add at JSON path.
    pub async fn json_numincrby_cmd(&self, key: String, path: String, increment: f64) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Document(doc) = &mut entry.value {
            let updated_json_value = jsonpath_numincrby(doc, &path, increment)?;
            entry.value = DataType::Document(updated_json_value.clone());
            self.touch_key(&key).await;
            Ok(DataType::Document(updated_json_value))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Document".to_string(), entry.value.data_type()))
        }
    }

    /// Command: JSON.STRAPPEND – Append to JSON string.
    pub async fn json_strappend_cmd(&self, key: String, path: String, value: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Document(doc) = &mut entry.value {
            let updated_json_value = jsonpath_strappend(doc, &path, &value)?;
            entry.value = DataType::Document(updated_json_value.clone());
            self.touch_key(&key).await;
            Ok(DataType::Document(updated_json_value))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Document".to_string(), entry.value.data_type()))
        }
    }

    /// Command: JSON.ARRAPPEND – Append elements to JSON array path.
    pub async fn json_arrappend_cmd(&self, key: String, path: String, values: Vec<String>) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Document(doc) = &mut entry.value {
            let json_values: Result<Vec<JsonValue>, CacheError> = values.iter()
                .map(|v_str| serde_json::from_str(v_str).map_err(|e| CacheError::InvalidCommandArguments(format!("Invalid JSON value in ARRAPPEND: {}", e))))
                .collect();
            let updated_json_value = jsonpath_arrappend(doc, &path, &json_values?)?;
            entry.value = DataType::Document(updated_json_value.clone());
            self.touch_key(&key).await;
            Ok(DataType::Document(updated_json_value))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Document".to_string(), entry.value.data_type()))
        }
    }

    /// Command: JSON.OBJSET – Set object field at path.
    pub async fn json_objset_cmd(&self, key: String, path: String, key_to_set: String, value: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();
        let value_json: JsonValue = serde_json::from_str(&value).map_err(|e| CacheError::InvalidCommandArguments(format!("Invalid JSON value in OBJSET: {}", e)))?;

        if let DataType::Document(doc) = &mut entry.value {
            let updated_json_value = jsonpath_objset(doc, &path, &key_to_set, value_json)?;
            entry.value = DataType::Document(updated_json_value.clone());
            self.touch_key(&key).await;
            Ok(DataType::Document(updated_json_value))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Document".to_string(), entry.value.data_type()))
        }
    }

    /// Command: JSON.OBJKEYS – List keys at object path.
    pub async fn json_objkeys_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let keys = jsonpath_objkeys(&doc, &path)?;
                Ok(DataType::List(keys.into_iter().collect()))
            },
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(key, "Document".to_string(), other.data_type())),
        }
    }

    /// Command: JSON.OBJLEN – Number of keys at object path.
    pub async fn json_objlen_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let len = jsonpath_objlen(&doc, &path)?;
                Ok(DataType::Integer(len as i64))
            },
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(key, "Document".to_string(), other.data_type())),
        }
    }

    /// Command: JSON.ARRINDEX – Find index of value (optionally within range).
    pub async fn json_arrindex_cmd(&self, key: String, path: String, value: String, range: Option<(isize, isize)>) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let value_json: JsonValue = serde_json::from_str(&value).map_err(|e| CacheError::InvalidCommandArguments(format!("Invalid JSON value in ARRINDEX: {}", e)))?;
                let index = jsonpath_arrindex(&doc, &path, &value_json, range)?;
                Ok(DataType::Integer(index as i64))
            },
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(key, "Document".to_string(), other.data_type())),
        }
    }

    /// Command: JSON.ARRINSERT – Insert into JSON array at index.
    pub async fn json_arrinsert_cmd(&self, key: String, path: String, index: isize, values: Vec<String>) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();
        let json_values: Result<Vec<JsonValue>, CacheError> = values.iter()
                .map(|v_str| serde_json::from_str(v_str).map_err(|e| CacheError::InvalidCommandArguments(format!("Invalid JSON value in ARRINSERT: {}", e))))
                .collect();

        if let DataType::Document(doc) = &mut entry.value {
            let updated_json_value = jsonpath_arrinsert(doc, &path, index, &json_values?)?;
            entry.value = DataType::Document(updated_json_value.clone());
            self.touch_key(&key).await;
            Ok(DataType::Document(updated_json_value))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Document".to_string(), entry.value.data_type()))
        }
    }

    /// Command: JSON.ARRLEN – Length of JSON array at path.
    pub async fn json_arrlen_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let len = jsonpath_arrlen(&doc, &path)?;
                Ok(DataType::Integer(len as i64))
            },
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(key, "Document".to_string(), other.data_type())),
        }
    }

    /// Command: JSON.ARRPOP – Pop element from JSON array path (optional index).
    pub async fn json_arrpop_cmd(&self, key: String, path: String, index: Option<isize>) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Document(doc) = &mut entry.value {
            let popped_value = jsonpath_arrpop(doc, &path, index)?;
            entry.value = DataType::Document(doc.clone());
            self.touch_key(&key).await;
            Ok(DataType::Document(popped_value))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Document".to_string(), entry.value.data_type()))
        }
    }

    /// Command: JSON.ARRTRIM – Trim JSON array.
    pub async fn json_arrtrim_cmd(&self, key: String, path: String, start: isize, stop: isize) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Document(doc) = &mut entry.value {
            let trimmed_len = jsonpath_arrtrim(doc, &path, start, stop)?;
            entry.value = DataType::Document(doc.clone());
            self.touch_key(&key).await;
            Ok(DataType::Integer(trimmed_len as i64))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Document".to_string(), entry.value.data_type()))
        }
    }

    // --- Graph Commands ---
    /// Command: GRAPH.CREATE_NODE – Create a node with optional properties.
    pub async fn graph_create_node_cmd(&self, key: String, node_id: String, properties: Vec<(String, String)>) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.clone()).or_insert_with(|| CacheEntry::new(DataType::Graph(GraphData::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::Graph(graph_data) = &mut entry.value {
            if graph_data.get_nodes().contains_key(&node_id) {
                return Err(CacheError::InvalidCommandArguments(format!("Node with ID '{}' already exists in graph {}", node_id, key)));
            }

            let node = Node::new(node_id.clone());
            for (prop_key, prop_value) in properties {
                node.set_property(prop_key, prop_value);
            }
            graph_data.add_node(node);
            self.touch_key(&key).await;
            Ok(DataType::String(OK_RESPONSE.to_string()))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), entry.value.data_type()))
        }
    }

    /// Command: GRAPH.GET_NODE – Fetch a node by id as JSON doc.
    pub async fn graph_get_node_cmd(&self, key: String, node_id: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Graph(graph_data) => {
                match graph_data.get_node(&node_id) {
                    Some(node_arc) => {
                        let node = Arc::try_unwrap(node_arc).unwrap();
                        Ok(DataType::Document(json!( {
                            "id": node.get_id(),
                            "properties": node.properties().iter().map(|entry| (entry.key().clone(), entry.value().clone())).collect::<HashMap<_,_>>()
                        })))
                    },
                    None => Ok(DataType::Nil),
                }
            },
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), other.data_type())),
        }
    }

    /// Command: GRAPH.DELETE_NODE – Remove a node (and implicit edges if handled upstream).
    pub async fn graph_delete_node_cmd(&self, key: String, node_id: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Graph(graph_data) = &mut entry.value {
            if graph_data.get_nodes().contains_key(&node_id) {
                graph_data.remove_node(&node_id);
                self.touch_key(&key).await;
                Ok(DataType::Integer(1))
            } else {
                Ok(DataType::Integer(0))
            }
        } else {
            Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), entry.value.data_type()))
        }
    }

    /// Command: GRAPH.CREATE_EDGE – Create an edge with properties.
    pub async fn graph_create_edge_cmd(&self, key: String, edge_id: String, source_node_id: String, traget_node_id: String, relation_type: String, properties: Vec<(String, String)>) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.clone()).or_insert_with(||CacheEntry::new(DataType::Graph(GraphData::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::Graph(graph_data) = &mut entry.value {
            if graph_data.get_edges().contains_key(&edge_id) {
                return Err(CacheError::InvalidCommandArguments(format!("Edge with ID '{}' already exists in graph '{}'", edge_id, key)));
            }

            if !graph_data.get_nodes().contains_key(&source_node_id) {
                return Err(CacheError::InvalidCommandArguments(format!("Source Node with ID '{}' does not exist in graph '{}'", source_node_id, key)));
            }

            if !graph_data.get_nodes().contains_key(&traget_node_id) {
                return Err(CacheError::InvalidCommandArguments(format!("Target Node with ID '{}' does not exist in graph '{}'", traget_node_id, key)));
            }

            let edge = Edge::new(edge_id.clone(), source_node_id.clone(), traget_node_id.clone(), relation_type.clone());
            for (prop_key, prop_value) in properties {
                edge.set_property(prop_key, prop_value);
            }
            graph_data.add_edge(edge);
            self.touch_key(&key).await;
            Ok(DataType::String(OK_RESPONSE.to_string()))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), entry.value.data_type()))
        }
    }


    /// Command: GRAPH.GET_EDGE – Fetch edge as JSON document.
    pub async fn graph_get_edge_cmd(&self, key: String, edge_id: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Graph(graph_data) => {
                match graph_data.get_edge(&edge_id) {
                    Some(edge_arc) => {
                        let edge = Arc::try_unwrap(edge_arc).unwrap();

                        Ok(DataType::Document(json!({
                            "id": edge.get_id(),
                            "source_id": edge.get_source_id(),
                            "target_id": edge.get_target_id(),
                            "relation_type": edge.get_relation_type(),
                            "properties": edge.properties().iter().map(|entry| (entry.key().clone(), entry.value().clone())).collect::<HashMap<_,_>>()
                        })))
                    },
                    None => Ok(DataType::Nil),
                }
            },
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), other.data_type()))
        }
    }

    /// Command: GRAPH.DELETE_EDGE – Remove edge by id.
    pub async fn graph_delete_edge_cmd(&self, key: String, edge_id: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Graph(graph_data) = &mut entry.value {
            if graph_data.get_edges().contains_key(&edge_id) {
                graph_data.remove_edge(&edge_id);
                self.touch_key(&key).await;
                Ok(DataType::Integer(1))
            } else {
                Ok(DataType::Integer(0))
            }
        } else {
            Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), entry.value.data_type()))
        }
    }

    /// Command: GRAPH.GET_NODE_PROPERTIES – Return node properties only.
    pub async fn graph_get_node_properties_cmd(&self, key: String, node_id: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Graph(graph_data) => {
                match graph_data.get_node(&node_id) {
                    Some(node_arc) => {
                        let node = Arc::try_unwrap(node_arc).unwrap();
                        let properties_map: HashMap<String, String> = node.properties().iter().map(|entry| (entry.key().clone(), entry.value().clone())).collect();
                        Ok(DataType::Document(json!(properties_map)))
                    },
                    None => Ok(DataType::Nil),
                }
            },
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), other.data_type()))
        }
    }

    /// Command: GRAPH.SET_NODE_PROPERTY – Upsert single property on node.
    pub async fn graph_set_node_property_cmd(&self, key: String, node_id: String, property_key: String, property_value: String) -> Result<DataType, CacheError>  {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Graph(graph_data) = &mut entry.value {
            match graph_data.get_node(&node_id) {
                Some(node_arc) => {
                    node_arc.set_property(property_key, property_value);
                    self.touch_key(&key).await;
                    Ok(DataType::String(OK_RESPONSE.to_string()))
                },
                None => Err(CacheError::InvalidCommandArguments(format!("Node with ID '{}' not found in graph '{}'", node_id, key)))
            }
        } else {
            Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), entry.value.data_type()))
        }
    }

    /// Command: GRAPH.DELETE_NODE_PROPERTY – Remove property from node if exists.
    pub async fn graph_delete_node_property_cmd(&self, key: String, node_id: String, property_key: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Graph(graph_data) = &mut entry.value {
            match graph_data.get_node(&node_id) {
                Some(node_arc) => {
                    node_arc.remove_property(&property_key);
                    self.touch_key(&key).await;
                    Ok(DataType::Integer(1))
                },
                None => Ok(DataType::Integer(0)) // Node not found, return 0 (property not deleted as node doesn't exist)
            }
        } else {
            Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), entry.value.data_type()))
        }
    }

    /// Command: GRAPH.GET_EDGE_PROPERTIES – Return edge properties only.
    pub async fn graph_get_edge_properties_cmd(&self, key: String, edge_id: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Graph(graph_data) => {
                match graph_data.get_edge(&edge_id) {
                    Some(edge_arc) => {
                        let edge = Arc::try_unwrap(edge_arc).unwrap();
                        let properties_map: HashMap<String, String> = edge.properties().iter().map(|entry| (entry.key().clone(), entry.value().clone())).collect();
                        Ok(DataType::Document(json!(properties_map))) // Return properties as JSON object
                    },
                    None => Ok(DataType::Nil), // Edge not found returns Nil
                }
            },
            DataType::Nil => Ok(DataType::Nil), // Graph key not found returns Nil
            other => Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), other.data_type())),
        }
    }

    /// Command: GRAPH.SET_EDGE_PROPERTY – Upsert property on edge.
    pub async fn graph_set_edge_property_cmd(&self, key: String, edge_id: String, property_key: String, property_value: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Graph(graph_data) = &mut entry.value {
            match graph_data.get_edge(&edge_id) {
                Some(edge_arc) => {
                    edge_arc.set_property(property_key, property_value);
                    self.touch_key(&key).await;
                    Ok(DataType::String(OK_RESPONSE.to_string())) // GRAPH.SET_EDGE_PROPERTY returns OK Simple String
                },
                None => Err(CacheError::InvalidCommandArguments(format!("Edge with ID '{}' not found in graph '{}'", edge_id, key))) // Edge not found is an error
            }
        } else {
            Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), entry.value.data_type()))
        }
    }

    /// Command: GRAPH.DELETE_EDGE_PROPERTY – Remove edge property if exists.
    pub async fn graph_delete_edge_property_cmd(&self, key: String, edge_id: String, property_key: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Graph(graph_data) = &mut entry.value {
            match graph_data.get_edge(&edge_id) {
                Some(edge_arc) => {
                    edge_arc.remove_property(&property_key);
                    self.touch_key(&key).await;
                    Ok(DataType::Integer(1))
                },
                None => Ok(DataType::Integer(0))
            }
        } else {
            Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), entry.value.data_type()))
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
        let mut hasher = ahash::AHasher::default();
        key.hash(&mut hasher);
        let hash = hasher.finish();
        self.touch_key_with_hash(hash).await;
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
        let mut hasher = ahash::AHasher::default();
        key.hash(&mut hasher);
        let hash = hasher.finish();
        self.remove_from_lru_with_hash(hash).await;
    }

    /// Perform eviction when over capacity using candidate ranking.
    pub async fn evict(&self) {
        use std::collections::BinaryHeap;

        let target_size = (self.capacity.load(Ordering::Relaxed) as f64 * 0.8) as usize;
    let current_size = self.size.load(Ordering::Relaxed);
    if current_size <= target_size { return; }

    let mut candidates = BinaryHeap::new();
        const MAX_CANDIDATES: usize = 50;

        if let Ok(lru) = self.lru_cache.try_read() {
            let iter = lru.iter().rev();
            for (key_hash, _) in iter.take(MAX_CANDIDATES) {
                // Find the actual key for this hash
                for entry_ref in self.data.iter() {
                    let mut hasher = ahash::AHasher::default();
                    entry_ref.key().hash(&mut hasher);
                    if hasher.finish() == *key_hash {
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
                    self.size.fetch_sub(1,Ordering::Relaxed);
                    self.metrics.total_data_size.fetch_sub(entry_size as u64, Ordering::Relaxed);
                    self.remove_from_lru(&candidate.key).await;
                }
            }
        }
    }
}


impl CalodShard {   
    /// Persist this shard's in-memory dataset to a binary snapshot at `path`.
    pub async fn save(&self, path: &str) -> Result<(), PersistenceError> {
        let data = self.data.iter().map(|entry| (entry.key().clone(), entry.value().clone())).collect::<Vec<_>>();

        let encoded = serialize(&data).map_err(PersistenceError::Serialization)?;

        write(path, &encoded).await.map_err(PersistenceError::Io)?;

        Ok(())
    }

    /// Load a snapshot from `path`, replacing (merging over) existing keys.
    /// Duplicate keys in the snapshot overwrite current values.
    pub async fn load(&self, path: &str) -> Result<(), PersistenceError> {
        let encoded = read(path).await.map_err(PersistenceError::Io)?;
        let data: Vec<(String, CacheEntry)> = deserialize(&encoded).map_err(PersistenceError::Serialization)?;

        for (key, entry) in data {
            self.data.insert(key, entry);
        }

        Ok(())
    }
}