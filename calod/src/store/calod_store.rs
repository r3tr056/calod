use chrono::Duration as ChronoDuration;
use dashmap::DashMap;
use tokio::sync::{broadcast, Mutex};
use tokio::time::timeout;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{Ordering, AtomicUsize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use ahash::AHasher;
use tracing::{error, trace};
use serde_json::{json, Error as SerdeJsonError, Value as JsonValue};

use bincode::{serialize, deserialize};
use tokio::fs::{read, write};
use crate::request_response::command::{InsertOption, SetExpireOption, SetOption};
use crate::store::calod_data::{CacheEntry, EvictionCandidate};

use super::calod_data::DataType;
use super::config::CacheConfig;
use super::graph::graph_data::{Edge, GraphData, Node};
use super::helpers::json_helpers::{json_type_to_string, jsonpath_arrappend, jsonpath_arrindex, jsonpath_arrinsert, jsonpath_arrlen, jsonpath_arrpop, jsonpath_arrtrim, jsonpath_del, jsonpath_get, jsonpath_numincrby, jsonpath_objkeys, jsonpath_objlen, jsonpath_objset, jsonpath_set, jsonpath_strappend};
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
    #[error("Data type mismatch for key `{0}`. Expected `{1}`, found `{2}`")]
    DataTypeMismatch(String, String, String),
    #[error("Field `{0}` not found in hash for key `{1}`")]
    FieldNotFound(String, String),
    #[error("Mutatuon `{0}` not supported")]
    MutationNotSupported(String),
    #[error("Invalid score format")]
    InvalidScoreFormat,
    #[error("Index out of range")]
    IndexOutOfRange,
    #[error("Value is not an integer")]
    NotAnInteger,
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

    #[inline]
    fn get_shard_index<K: Hash + ?Sized>(&self, key: &K) -> ShardIndex where K:AsRef<[u8]> {
        let mut hasher = AHasher::default();
        key.hash(&mut hasher);
        (hasher.finish() % self.shards.len() as u64) as usize
    }

    pub async fn ping(&self) -> String {
        "+PONG\r\n".to_string()
    }

    pub async fn info(&self, _section: Option<&str>) -> String {
        let metrics_report = self.metrics.report();
        format!("{}\r\n", metrics_report)
    }

    // generic commands
    pub async fn type_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].type_cmd(key).await
    }

    pub async fn keys(&self, pattern: &str) -> Vec<String> {
        let mut all_keys = Vec::new();
        for shard in &self.shards {
            all_keys.extend(shard.keys(pattern).await);
        }
        all_keys
    }

    pub async fn exists(&self, key: &str) -> bool {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].exists(key).await
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

    pub async fn delete(&self, key: &str) -> bool {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].delete(key).await
    }

    pub async fn keys_cmd(&self, pattern: &str) -> Result<DataType, CacheError> {
        let keys = self.keys(pattern).await;
        let list_data: Vec<String> = keys.into_iter().collect();
        Ok(DataType::List(list_data.into()))
    }

    pub async fn exists_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let exists = self.exists(key).await;
        Ok(DataType::Integer(if exists { 1 } else { 0 }))
    }

    pub async fn expire_cmd(&self, key: &str, seconds: u64) -> Result<DataType, CacheError> {
        let expired = self.expire(key, seconds).await?;
        Ok(DataType::Integer(if expired { 1 } else { 0 }))
    }

    pub async fn ttl_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let ttl_result = self.ttl(key).await?;
        Ok(DataType::Integer(ttl_result.unwrap_or(-2)))
    }

    pub async fn persist_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let persisted = self.persist(key).await?;
        Ok(DataType::Integer(if persisted { 1 } else { 0 }))
    }

    pub async fn del_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let deleted_count = if self.delete(key).await { 1 } else { 0 };
        Ok(DataType::Integer(deleted_count))
    }

    // strings/numbers commands

    pub async fn get_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let result = self.get(key).await;
        match result {
            Ok(DataType::String(_)) | Ok(DataType::Nil) => result, // Only return Strings or Nil for GET
            Ok(other_type) => Err(CacheError::DataTypeMismatch(key.to_string(), "string".to_string(), other_type.data_type())),
            Err(e) => Err(e)
        }
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

    pub async fn set_cmd(&self, key: String, value: String, expire_option: Option<SetExpireOption>, set_option: Option<SetOption>) -> Result<(), CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].set_cmd(key, DataType::String(value), expire_option, set_option).await
    }

    pub async fn append_cmd(&self, key: &str, value: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].append_cmd(key, value).await
    }

    pub async fn strlen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].strlen_cmd(key).await
    }

    pub async fn getrange_cmd(&self, key: &str, start: isize, end: isize) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].getrange_cmd(key, start, end).await
    }

    pub async fn setrange_cmd(&self, key: &str, offset: usize, value: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].setrange_cmd(key, offset, value).await
    }

    pub async fn getset_cmd(&self, key: &str, value: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].getset_cmd(key, value).await
    }

    pub async fn mget_cmd(&self, keys: &Vec<String>) -> Result<DataType, CacheError> {
        let mut results = Vec::with_capacity(keys.len());
        for key in keys {
            let shard_idx = self.get_shard_index(key);
            results.push(self.shards[shard_idx].get(key).await);
        }

        let resp_list = results.into_iter().map(|res| match res {
            Ok(DataType::String(s)) => s,
            Ok(DataType::Nil) | Err(CacheError::KeyNotFound(_)) => "".to_string(),
            Err(e) => {
                error!("Error during MGET: {:?}", e);
                format!("Error during MGET: {:?}", e)
            }
            Ok(other) => {
                error!("Unexpected DataType in MGET: {:?}", other);
                format!("Unexpected DataType in MGET: {:?}", other)
            }
        }).collect::<Vec<String>>();

        Ok(DataType::List(resp_list))
    }

    pub async fn mset_cmd(&self, key_values: Vec<(String, String)>) -> Result<(), CacheError> {
        for (key, value) in key_values {
            let shard_idx = self.get_shard_index(&key);
            self.shards[shard_idx].set_cmd(key, DataType::String(value), None, None).await?; // No expiry/option for MSET in this example
        }
        Ok(())
    }

    pub async fn incr_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].incr_cmd(key).await
    }

    pub async fn decr_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].decr_cmd(key).await
    }

    pub async fn incrby_cmd(&self, key: &str, increment: i64) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].incrby_cmd(key, increment).await
    }

    pub async fn decrby_cmd(&self, key: &str, decrement: i64) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].decrby_cmd(key, decrement).await
    }

    pub async fn incrbyfloat_cmd(&self, key: &str, increment: f64) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].incrbyfloat_cmd(key, increment).await
    }

    // Hash Commands
    pub async fn hset_cmd(&self, key: &str, field_values: Vec<(String, String)>) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hset_cmd(key, field_values).await
    }
    pub async fn hget_cmd(&self, key: &str, field: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hget_cmd(key, field).await
    }

    pub async fn hdel_cmd(&self, key: &str, fields: Vec<String>) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hdel_cmd(key, fields).await
    }

    pub async fn hexists_cmd(&self, key: &str, field: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hexists_cmd(key, field).await
    }

    pub async fn hgetall_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hgetall_cmd(key).await
    }

    pub async fn hincrby_cmd(&self, key: &str, field: String, increment: i64) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hincrby_cmd(key, field, increment).await
    }

    pub async fn hincrbyfloat_cmd(&self, key: &str, key_field_increment: Vec<(String, f64)>) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hincrbyfloat_cmd(key, key_field_increment).await
    }

    pub async fn hkeys_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hkeys_cmd(key).await
    }

    pub async fn hlen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hlen_cmd(key).await
    }

    pub async fn hmget_cmd(&self, key: &str, fields: Vec<String>) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hmget_cmd(key, fields).await
    }

    pub async fn hmset_cmd(&self, key: &str, field_values: Vec<(String, String)>) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hmset_cmd(key, field_values).await
    }

    pub async fn hsetnx_cmd(&self, key: &str, field: String, value: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hsetnx_cmd(key, field, value).await
    }

    pub async fn hvals_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].hvals_cmd(key).await
    }

    // List Commands
    pub async fn lpush_cmd(&self, key: String, values: Vec<String>) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].lpush_cmd(key, values).await
    }

    pub async fn rpush_cmd(&self, key: String, values: Vec<String>) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].rpush_cmd(key, values).await
    }

    pub async fn lpop_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].lpop_cmd(key).await
    }

    pub async fn rpop_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].rpop_cmd(key).await
    }

    pub async fn llen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].llen_cmd(key).await
    }

    pub async fn lrange_cmd(&self, key: &str, start: isize, end: isize) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].lrange_cmd(key, start, end).await
    }

    pub async fn lindex_cmd(&self, key: &str, index: isize) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].lindex_cmd(key, index).await
    }

    pub async fn linsert_cmd(&self, key: &str, before_after: InsertOption, pivot: String, value: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].linsert_cmd(key, before_after, pivot, value).await
    }

    pub async fn lset_cmd(&self, key: &str, index: isize, value: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].lset_cmd(key, index, value).await
    }

    pub async fn ltrim_cmd(&self, key: &str, start: isize, end: isize) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].ltrim_cmd(key, start, end).await
    }

    pub async fn lrem_cmd(&self, key: &str, count: i64, value: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(key);
        self.shards[shard_idx].lrem_cmd(key, count, value).await
    }

    pub async fn rpoplpush_cmd(&self, source: &str, destination: &str) -> Result<DataType, CacheError> {
        let source_shard_idx = self.get_shard_index(source);
        let dest_shard_idx = self.get_shard_index(destination);

        // For RPOPLPUSH, we need to access two shards. To avoid deadlock, we can acquire locks in a consistent order
        let (source_shard, _dest_shard) = if source_shard_idx < dest_shard_idx {
            (&self.shards[source_shard_idx], &self.shards[dest_shard_idx])
        } else if source_shard_idx > dest_shard_idx {
            (&self.shards[dest_shard_idx], &self.shards[source_shard_idx])
        } else { // Same shard, no need for special ordering
            (&self.shards[source_shard_idx], &self.shards[dest_shard_idx])
        };

        // Note: Directly calling shard methods here, as RPOPLPUSH logic itself needs to handle cross-shard operations if needed.
        source_shard.rpoplpush_cmd(source, destination).await
    }

    pub async fn blpop_cmd(&self, keys: Vec<String>, timeout: f64) -> Result<DataType, CacheError> {
        // BLPOP/BRPOP can operate on multiple keys, but within the same shard in this implementation.
        // Choose the shard based on the first key for simplicity.  Redis allows BLPOP/BRPOP across multiple keys in different DBs, but here we assume keys are within the same DB (shard group).
        if let Some(first_key) = keys.first() {
            let shard_idx = self.get_shard_index(first_key);
            self.shards[shard_idx].blpop_cmd(keys, timeout).await
        } else {
            Ok(DataType::Nil) // No keys provided, return Nil immediately
        }
    }

    pub async fn brpop_cmd(&self, keys: Vec<String>, timeout: f64) -> Result<DataType, CacheError> {
         if let Some(first_key) = keys.first() {
            let shard_idx = self.get_shard_index(first_key);
            self.shards[shard_idx].brpop_cmd(keys, timeout).await
        } else {
            Ok(DataType::Nil) // No keys provided, return Nil immediately
        }
    }

    // --- JSON Commands ---
    pub async fn json_set_cmd(&self, key: String, path: String, value: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_set_cmd(key, path, value).await
    }

    pub async fn json_get_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_get_cmd(key, path).await
    }

    pub async fn json_del_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_del_cmd(key, path).await
    }

    pub async fn json_type_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_type_cmd(key, path).await
    }

    pub async fn json_numincrby_cmd(&self, key: String, path: String, increment: f64) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_numincrby_cmd(key, path, increment).await
    }

    pub async fn json_strappend_cmd(&self, key: String, path: String, value: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_strappend_cmd(key, path, value).await
    }

    pub async fn json_arrappend_cmd(&self, key: String, path: String, values: Vec<String>) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_arrappend_cmd(key, path, values).await
    }

    pub async fn json_objset_cmd(&self, key: String, path: String, key_to_set: String, value: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_objset_cmd(key, path, key_to_set, value).await
    }

    pub async fn json_objkeys_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_objkeys_cmd(key, path).await
    }

    pub async fn json_objlen_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_objlen_cmd(key, path).await
    }

    pub async fn json_arrindex_cmd(&self, key: String, path: String, value: String, range: Option<(isize, isize)>) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_arrindex_cmd(key, path, value, range).await
    }

    pub async fn json_arrinsert_cmd(&self, key: String, path: String, index: isize, values: Vec<String>) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_arrinsert_cmd(key, path, index, values).await
    }

    pub async fn json_arrlen_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_arrlen_cmd(key, path).await
    }

    pub async fn json_arrpop_cmd(&self, key: String, path: String, index: Option<isize>) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_arrpop_cmd(key, path, index).await
    }

    pub async fn json_arrtrim_cmd(&self, key: String, path: String, start: isize, stop: isize) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].json_arrtrim_cmd(key, path, start, stop).await
    }

    // --- Graph Commands ---
    pub async fn graph_create_node_cmd(&self, key: String, node_id: String, properties: Vec<(String, String)>) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].graph_create_node_cmd(key, node_id, properties).await
    }

    pub async fn graph_get_node_cmd(&self, key: String, node_id: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].graph_get_node_cmd(key, node_id).await
    }

    pub async fn graph_delete_node_cmd(&self, key: String, node_id: String) -> Result<DataType, CacheError> {
        let shard_idx = self.get_shard_index(&key);
        self.shards[shard_idx].graph_delete_node_cmd(key, node_id).await
    }

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
    list_modification_notifier: broadcast::Sender<String>,
}

impl CalodShard {
    fn new(capacity: usize, metrics: Arc<CalodMetrics>) -> Self {
        Self {
            data: DashMap::new(),
            lru: Mutex::new(VecDeque::new()),
            capacity: AtomicUsize::new(capacity),
            size: AtomicUsize::new(0),
            list_modification_notifier: broadcast::channel(32).0,
            metrics,
        }
    }

    // Command: TYPE, Returns the datatype
    pub async fn type_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        match self.data.get(key) {
            Some(entry_ref) => {
                let entry = entry_ref.value();
                if entry.is_expired() {
                    drop(entry_ref);
                    self.data.remove(key);
                    self.size.fetch_sub(1, Ordering::Relaxed);
                    self.remove_from_lru(key).await;
                    return Ok(DataType::Nil);
                }
                Ok(DataType::String(entry.value.data_type()))
            },
            None => Ok(DataType::String("none".to_string()))
        }
    }

    // Command: KEYS, Returns the keys in the store matching a regex pattern string
    async fn keys(&self, pattern: &str) -> Vec<String> {
        trace!("Shard getting keys with pattern: {}", pattern);
        self.data.iter().filter_map(|entry| {
            if entry.key().contains(pattern) && !entry.value().is_expired() {
                Some(entry.key().clone())
            } else {
                None
            }
        }).collect()
    }

    // Command: EXISTS, Returns whether a key exists in the datastore
    async fn exists(&self, key: &str) -> bool {
        trace!("Shard checking exists for key: {}", key);
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

    // Command: EXPIRE, Expires a key with a set timeout
    async fn exipre(&self, key: &str, seconds: u64) -> Result<bool, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        entry_writer.value_mut().expire_in(Duration::from_secs(seconds));
        Ok(true)
    }

    // Command: TTL
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

    // Command : PERSIST
    async fn persist(&self, key: &str) -> Result<bool, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        entry_writer.value_mut().persist();
        Ok(true)
    }

    // Command : DEL
    async fn delete(&self, key: &str) -> bool {
        trace!("Shard deleting key: {}", key);
        if self.data.remove(key).is_some() {
            self.size.fetch_sub(1, Ordering::Relaxed);
            self.remove_from_lru(key).await;
            true
        } else {
            false
        }
    }

    // Strings/Numbers Commands
    // Command: GET
    async fn get(&self, key: &str) -> Result<DataType, CacheError> {
        trace!("Shard getting key: {}", key);
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

    // Command: SET
    pub async fn set_cmd(&self, key: String, value: DataType, expire_option: Option<SetExpireOption>, set_option: Option<SetOption>) -> Result<(), CacheError> {
        let mut ttl_duration: Option<ChronoDuration> = None;
        if let Some(expire) = expire_option {
            match expire {
                SetExpireOption::EX(seconds) => ttl_duration = Some(ChronoDuration::seconds(seconds as i64)),
                SetExpireOption::PX(milliseconds) => ttl_duration = Some(ChronoDuration::milliseconds(milliseconds as i64)),
            }
        }

        if let Some(option) = set_option {
            match option {
                SetOption::NX => { if self.exists(&key).await { return Ok(()); } },
                SetOption::XX => { if !self.exists(&key).await { return Ok(()); } }
            }
        }

        self.set(key, value, ttl_duration).await;
        Ok(())
    }

    // Command: SET
    async fn set(&self, key: String, value: DataType, ttl: Option<ChronoDuration>) -> Option<DataType> {
        trace!("Shard setting key: {}", key);
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

    // Command : APPEND
    pub async fn append_cmd(&self, key: &str, value: String) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.to_string()).or_insert_with(|| CacheEntry::new(DataType::String("".into()), None));
        let entry = entry_mut.value_mut();

        if let DataType::String(s) = &mut entry.value {
            s.push_str(&value);
            let new_len = s.len() as i64;
            self.touch_key(key).await;
            Ok(DataType::Integer(new_len))
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "String".to_string(), entry.value.data_type()))
        }
    }

    // Command: STRLEN
    pub async fn strlen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        match self.get(key).await {
            Ok(DataType::String(s)) => Ok(DataType::Integer(s.len() as i64)),
            Ok(DataType::Nil) => Ok(DataType::Integer(0)), // Key doesn't exist, STRLEN is 0
            Ok(other_type) => Err(CacheError::DataTypeMismatch(key.to_string(), "string".to_string(), other_type.data_type())),
            Err(e) => Err(e)
        }
    }

    // Command GETRANGE
    pub async fn getrange_cmd(&self, key: &str, start: isize, end: isize) -> Result<DataType, CacheError> {
        match self.get(key).await {
            Ok(DataType::String(s)) => {
                let len = s.len() as isize;
                let start_index = if start < 0 { (start + len).max(0) } else { start }.min(len);
                let end_index = if end < 0 { (end + len).max(0) } else { end }.min(len);

                if start_index > end_index {
                    return Ok(DataType::String("".to_string())); // Empty range
                }

                let range = s[start_index as usize..end_index as usize + 1].to_string();
                Ok(DataType::String(range))
            },
            Ok(DataType::Nil) => Ok(DataType::String("".to_string())), // Key doesn't exist, GETRANGE returns empty string
            Ok(other_type) => Err(CacheError::DataTypeMismatch(key.to_string(), "string".to_string(), other_type.data_type())),
            Err(e) => Err(e)
        }
    }

    // Command SETRANGE
    pub async fn setrange_cmd(&self, key: &str, offset: usize, value: String) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.to_string()).or_insert_with(|| CacheEntry::new(DataType::String("".into()), None)); //Create empty string if key not exists
        let entry = entry_mut.value_mut();

        if let DataType::String(s) = &mut entry.value {
            if offset > s.len() {
                // Pad with zero bytes if offset is beyond current length
                let padding = offset - s.len();
                s.extend(std::iter::repeat('\0').take(padding));
            }
            let mut s_bytes = s.as_bytes().to_vec();
            let value_bytes = value.as_bytes();

            for (i, &byte) in value_bytes.iter().enumerate() {
                if offset + i < s_bytes.len() {
                    s_bytes[offset + i] = byte;
                } else {
                    s_bytes.push(byte);
                }
            }

            if let Ok(updated_s) = String::from_utf8(s_bytes) {
                entry.value = DataType::String(updated_s);
                let new_len = entry.value.size() as i64;
                self.touch_key(key).await;
                Ok(DataType::Integer(new_len))
            } else {
                Err(CacheError::InternalError("Failed to convert updated bytes back to String".to_string()))
            }


        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "String".to_string(), entry.value.data_type()))
        }
    }

    // Command : GETSET
    pub async fn getset_cmd(&self, key: &str, value: String) -> Result<DataType, CacheError> {
        let existing_value = self.get(key).await.unwrap_or(DataType::Nil); // Get existing value, default to Nil if not found
        self.set_cmd(key.to_string(), DataType::String(value), None, None).await?; // Set new value
        Ok(existing_value) // Return the old value
    }

    // Command : INCR
    async fn incr_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        match &mut entry.value {
            DataType::Integer(n) => {
                *n += 1;
                self.touch_key(key).await;
                Ok(DataType::Integer(*n))
            }
            DataType::String(s) => {
                match s.parse::<i64>() {
                    Ok(mut n) => {
                        n += 1;
                        entry.value = DataType::Integer(n);
                        self.touch_key(key).await;
                        Ok(DataType::Integer(n))
                    }
                    Err(_) => Err(CacheError::NotAnInteger),
                }
            },
            _ => Err(CacheError::DataTypeMismatch(key.to_string(), "Integer or String representable as integer".to_string(), entry.value.data_type())),
        }
    }

    // Command : DECR
    async fn decr_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        match &mut entry.value {
            DataType::Integer(n) => {
                *n -= 1;
                self.touch_key(key).await;
                Ok(DataType::Integer(*n))
            },
            DataType::String(s) => {
                match s.parse::<i64>() {
                    Ok(mut n) => {
                        n -= 1;
                        entry.value = DataType::Integer(n);
                        self.touch_key(key).await;
                        Ok(DataType::Integer(n))
                    }
                    Err(_) => Err(CacheError::NotAnInteger)
                }
            },
            _ => Err(CacheError::DataTypeMismatch(key.to_string(), "Integer or String representable as integer".to_string(), entry.value.data_type())),
        }
    }

    // Command: INCRBY
    pub async fn incrby_cmd(&self, key: &str, increment: i64) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        match &mut entry.value {
            DataType::Integer(n) => {
                *n += increment;
                self.touch_key(key).await;
                Ok(DataType::Integer(*n))
            }
            DataType::String(s) => {
                match s.parse::<i64>() {
                    Ok(mut n) => {
                        n += increment;
                        entry.value = DataType::Integer(n);
                        self.touch_key(key).await;
                        Ok(DataType::Integer(n))
                    }
                    Err(_) => Err(CacheError::NotAnInteger)
                }
            }
            DataType::Nil => { // Treat Nil as 0 for INCRBY
                let result = increment;
                entry.value = DataType::Integer(result);
                self.touch_key(key).await;
                Ok(DataType::Integer(result))
            }
            _ => Err(CacheError::DataTypeMismatch(key.to_string(), "Integer or String representable as integer".to_string(), entry.value.data_type())),
        }
    }

    // Command : DECRBY
    pub async fn decrby_cmd(&self, key: &str, decrement: i64) -> Result<DataType, CacheError> {
        // Reuse incrby_cmd with negative increment for DECRBY
        self.incrby_cmd(key, -decrement).await
    }

    // Command : INCRBYFLOAT
    pub async fn incrbyfloat_cmd(&self, key: &str, increment: f64) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        match &mut entry.value {
            DataType::String(s) => {
                match s.parse::<f64>() {
                    Ok(mut n) => {
                        n += increment;
                        entry.value = DataType::String(format!("{}", n)); // Store as string for INCRBYFLOAT
                        self.touch_key(key).await;
                        Ok(DataType::String(format!("{}", n)))
                    }
                    Err(_) => Err(CacheError::InvalidScoreFormat)
                }
            }
            DataType::Nil => { // Treat Nil as 0.0 for INCRBYFLOAT
                let result = increment;
                entry.value = DataType::String(format!("{}", result));
                self.touch_key(key).await;
                Ok(DataType::String(format!("{}", result)))
            }
            _ => Err(CacheError::DataTypeMismatch(key.to_string(), "String representable as float".to_string(), entry.value.data_type())),
        }
    }

    // Hash Commands
    // Command: HSET
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

    pub async fn hmset_cmd(&self, key: &str, field_values: Vec<(String, String)>) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.to_string()).or_insert_with(|| CacheEntry::new(DataType::Hash(DashMap::new()), None));
        let entry = entry_mut.value_mut();

        if let DataType::Hash(hash_map) = &mut entry.value {
            for (field, value) in field_values {
                hash_map.insert(field, value);
            }
            self.touch_key(key).await;
            Ok(DataType::String("OK".to_string())) // HMSET returns OK Simple String
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "Hash".to_string(), entry.value.data_type()))
        }
    }

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
            Ok(DataType::String("OK".to_string())) // LSET returns OK Simple String
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "List".to_string(), entry.value.data_type()))
        }
    }

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
            Ok(DataType::String("OK".to_string())) // LTRIM returns OK Simple String
        } else {
            Err(CacheError::DataTypeMismatch(key.to_string(), "List".to_string(), entry.value.data_type()))
        }
    }
    
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

    // --- JSON Command Handlers ---
    async fn json_set_cmd(&self, key: String, path: String, value_str: String) -> Result<DataType, CacheError> {
        let mut entry_mut = self.data.entry(key.clone()).or_insert_with(|| CacheEntry::new(DataType::Document(JsonValue::Null), None));
        let entry = entry_mut.value_mut();

        let json_value: JsonValue = serde_json::from_str(&value_str).map_err(|e: SerdeJsonError| CacheError::InvalidCommandArguments(format!("Invalid JSON value: {}", e)))?;

        if let DataType::Document(doc) = &mut entry.value {
            let result_json = jsonpath_set(doc, &path, json_value)?;
            entry.value = DataType::Document(result_json);
            self.touch_key(&key).await;
            Ok(DataType::String("OK".to_string()))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Document".to_string(), entry.value.data_type()))
        }
    }

    async fn json_get_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let result_json_value = jsonpath_get(&doc, &path)?;
                Ok(DataType::Document(result_json_value))
            },
            DataType::Nil => Ok(DataType::Nil), // Key not found returns Nil
            other => Err(CacheError::DataTypeMismatch(key, "Document".to_string(), other.data_type())),
        }
    }

    async fn json_del_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
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

    async fn json_type_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let json_value = jsonpath_get(&doc, &path)?;
                Ok(DataType::String(json_type_to_string(&json_value)))
            },
            DataType::Nil => Ok(DataType::Nil), // Key not found returns Nil
            other => Err(CacheError::DataTypeMismatch(key, "Document".to_string(), other.data_type())),
        }
    }

    async fn json_numincrby_cmd(&self, key: String, path: String, increment: f64) -> Result<DataType, CacheError> {
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

    async fn json_strappend_cmd(&self, key: String, path: String, value: String) -> Result<DataType, CacheError> {
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

    async fn json_arrappend_cmd(&self, key: String, path: String, values: Vec<String>) -> Result<DataType, CacheError> {
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

    async fn json_objset_cmd(&self, key: String, path: String, key_to_set: String, value: String) -> Result<DataType, CacheError> {
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

    async fn json_objkeys_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let keys = jsonpath_objkeys(&doc, &path)?;
                Ok(DataType::List(keys.into_iter().collect()))
            },
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(key, "Document".to_string(), other.data_type())),
        }
    }

    async fn json_objlen_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let len = jsonpath_objlen(&doc, &path)?;
                Ok(DataType::Integer(len as i64))
            },
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(key, "Document".to_string(), other.data_type())),
        }
    }

    async fn json_arrindex_cmd(&self, key: String, path: String, value: String, range: Option<(isize, isize)>) -> Result<DataType, CacheError> {
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

    async fn json_arrinsert_cmd(&self, key: String, path: String, index: isize, values: Vec<String>) -> Result<DataType, CacheError> {
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

    async fn json_arrlen_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        match self.get(&key).await? {
            DataType::Document(doc) => {
                let len = jsonpath_arrlen(&doc, &path)?;
                Ok(DataType::Integer(len as i64))
            },
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(key, "Document".to_string(), other.data_type())),
        }
    }

    async fn json_arrpop_cmd(&self, key: String, path: String, index: Option<isize>) -> Result<DataType, CacheError> {
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

    async fn json_arrtrim_cmd(&self, key: String, path: String, start: isize, stop: isize) -> Result<DataType, CacheError> {
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
    async fn graph_create_node_cmd(&self, key: String, node_id: String, properties: Vec<(String, String)>) -> Result<DataType, CacheError> {
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
            Ok(DataType::String("OK".to_string()))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), entry.value.data_type()))
        }
    }

    async fn graph_get_node_cmd(&self, key: String, node_id: String) -> Result<DataType, CacheError> {
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

    async fn graph_delete_node_cmd(&self, key: String, node_id: String) -> Result<DataType, CacheError> {
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

    async fn graph_create_edge_cmd(&self, key: String, edge_id: String, source_node_id: String, traget_node_id: String, relation_type: String, properties: Vec<(String, String)>) -> Result<DataType, CacheError> {
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
            Ok(DataType::String("OK".to_string()))
        } else {
            Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), entry.value.data_type()))
        }
    }


    async fn graph_get_edge_cmd(&self, key: String, edge_id: String) -> Result<DataType, CacheError> {
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

    async fn graph_delete_edge_cmd(&self, key: String, edge_id: String) -> Result<DataType, CacheError> {
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

    async fn graph_get_node_properties_cmd(&self, key: String, node_id: String) -> Result<DataType, CacheError> {
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

    async fn graph_set_node_property_cmd(&self, key: String, node_id: String, property_key: String, property_value: String) -> Result<DataType, CacheError>  {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Graph(graph_data) = &mut entry.value {
            match graph_data.get_node(&node_id) {
                Some(node_arc) => {
                    node_arc.set_property(property_key, property_value);
                    self.touch_key(&key).await;
                    Ok(DataType::String("OK".to_string()))
                },
                None => Err(CacheError::InvalidCommandArguments(format!("Node with ID '{}' not found in graph '{}'", node_id, key)))
            }
        } else {
            Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), entry.value.data_type()))
        }
    }

    async fn graph_delete_node_property_cmd(&self, key: String, node_id: String, property_key: String) -> Result<DataType, CacheError> {
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

    async fn graph_get_edge_properties_cmd(&self, key: String, edge_id: String) -> Result<DataType, CacheError> {
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

    async fn graph_set_edge_property_cmd(&self, key: String, edge_id: String, property_key: String, property_value: String) -> Result<DataType, CacheError> {
        let mut entry_writer = self.data.get_mut(&key).ok_or_else(|| CacheError::KeyNotFound(key.to_string()))?;
        let entry = entry_writer.value_mut();

        if let DataType::Graph(graph_data) = &mut entry.value {
            match graph_data.get_edge(&edge_id) {
                Some(edge_arc) => {
                    edge_arc.set_property(property_key, property_value);
                    self.touch_key(&key).await;
                    Ok(DataType::String("OK".to_string())) // GRAPH.SET_EDGE_PROPERTY returns OK Simple String
                },
                None => Err(CacheError::InvalidCommandArguments(format!("Edge with ID '{}' not found in graph '{}'", edge_id, key))) // Edge not found is an error
            }
        } else {
            Err(CacheError::DataTypeMismatch(key, "Graph".to_string(), entry.value.data_type()))
        }
    }

    async fn graph_delete_edge_property_cmd(&self, key: String, edge_id: String, property_key: String) -> Result<DataType, CacheError> {
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


impl CalodShard {    
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
}