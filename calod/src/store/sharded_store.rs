use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;
use ahash::AHasher;
use tracing::{error, info};
use crate::cld_srv::command::{InsertOption, SetExpireOption, SetOption};

use super::calod_data::DataType;
use super::calod_shard::CalodShard;
use super::config::CalodConfig;
use super::error::CacheError;
use super::metrics::CalodMetrics;


type ShardIndex = usize;

pub struct ShardedStore {
    shards: Vec<Arc<CalodShard>>,
    metrics: Arc<CalodMetrics>,
    config: Arc<CalodConfig>
}

impl ShardedStore {
    /// Create a new sharded store with the given configuration
    pub fn new(config: CalodConfig) -> Self {
        let num_shards = config.shards;
        let shard_capacity = config.shard_capacity / num_shards;
        let metrics = Arc::new(CalodMetrics::default());
        
        info!("Creating ShardedStore with {} shards, {} capacity each", 
              num_shards, shard_capacity);
        
        let mut shards = Vec::with_capacity(num_shards);
        for i in 0..num_shards {
            shards.push(Arc::new(CalodShard::new(
                shard_capacity,
                metrics.clone(),
                i
            )));
        }
        
        Self {
            shards,
            metrics,
            config: Arc::new(config),
        }
    }

    /// Get the shard index for a given key
    pub fn get_shard_index<K: Hash + ?Sized>(&self, key: &K) -> ShardIndex where K:AsRef<[u8]> {
        let mut hasher = AHasher::default();
        key.hash(&mut hasher);
        (hasher.finish() % self.shards.len() as u64) as usize
    }

    /// Get a reference to the shard that contains the given key
    pub fn get_shard(&self, key: &str) -> Arc<CalodShard> {
        let index = self.get_shard_index(key);
        self.shards[index].clone()
    }

    /// Get a reference to all shards
    pub fn get_all_shards(&self) -> Vec<Arc<CalodShard>> {
        self.shards.clone()
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
        let shard = self.get_shard(key);
        shard.type_cmd(key).await
    }

    pub async fn keys(&self, pattern: &str) -> Vec<String> {
        let mut all_keys = Vec::new();
        let mut futures = Vec::new();
        
        for shard in &self.shards {
            let shard = shard.clone();
            let pattern = pattern.to_string();
            let future = tokio::spawn(async move {
                shard.keys(&pattern).await
            });
            futures.push(future);
        }

        // Collect results
        for future in futures {
            if let Ok(keys) = future.await {
                all_keys.extend(keys);
            }
        }
        
        all_keys
    }

    pub async fn exists(&self, key: &str) -> bool {
        let shard = self.get_shard(key);
        shard.exists(key).await
    }

    pub async fn expire(&self, key: &str, seconds: u64) -> Result<bool, CacheError> {
        let shard = self.get_shard(key);
        shard.expire(key, seconds).await
    }

    pub async fn ttl(&self, key: &str) -> Result<Option<i64>, CacheError> {
        let shard = self.get_shard(key);
        shard.ttl(key).await
    }

    pub async fn persist(&self, key: &str) -> Result<bool, CacheError> {
        let shard = self.get_shard(key);
        shard.persist(key).await
    }

    pub async fn delete(&self, key: &str) -> bool {
        let shard = self.get_shard(key);
        shard.delete(key).await
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
        let shard = self.get_shard(key);
        let result = shard.get(key).await;

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
        let shard = self.get_shard(&key);
        shard.set_cmd(key, DataType::String(value), expire_option, set_option).await
    }

    pub async fn append_cmd(&self, key: &str, value: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.append_cmd(key, value).await
    }

    pub async fn strlen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.strlen_cmd(key).await
    }

    pub async fn getrange_cmd(&self, key: &str, start: isize, end: isize) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.getrange_cmd(key, start, end).await
    }

    pub async fn setrange_cmd(&self, key: &str, offset: usize, value: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.setrange_cmd(key, offset, value).await
    }

    pub async fn getset_cmd(&self, key: &str, value: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.getset_cmd(key, value).await
    }

    pub async fn mget_cmd(&self, keys: &Vec<String>) -> Result<DataType, CacheError> {
        let mut results = Vec::with_capacity(keys.len());
        for key in keys {
            let shard = self.get_shard(key);
            results.push(shard.get(key).await);
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
            let shard = self.get_shard(&key);
            shard.set_cmd(key, DataType::String(value), None, None).await?; // No expiry/option for MSET in this example
        }
        Ok(())
    }

    pub async fn incr_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.incr_cmd(key).await
    }

    pub async fn decr_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.decr_cmd(key).await
    }

    pub async fn incrby_cmd(&self, key: &str, increment: i64) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.incrby_cmd(key, increment).await
    }

    pub async fn decrby_cmd(&self, key: &str, decrement: i64) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.decrby_cmd(key, decrement).await
    }

    pub async fn incrbyfloat_cmd(&self, key: &str, increment: f64) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.incrbyfloat_cmd(key, increment).await
    }

    // Hash Commands
    pub async fn hset_cmd(&self, key: &str, field_values: Vec<(String, String)>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hset_cmd(key, field_values).await
    }
    pub async fn hget_cmd(&self, key: &str, field: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hget_cmd(key, field).await
    }

    pub async fn hdel_cmd(&self, key: &str, fields: Vec<String>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hdel_cmd(key, fields).await
    }

    pub async fn hexists_cmd(&self, key: &str, field: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hexists_cmd(key, field).await
    }

    pub async fn hgetall_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hgetall_cmd(key).await
    }

    pub async fn hincrby_cmd(&self, key: &str, field: String, increment: i64) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hincrby_cmd(key, field, increment).await
    }

    pub async fn hincrbyfloat_cmd(&self, key: &str, key_field_increment: Vec<(String, f64)>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hincrbyfloat_cmd(key, key_field_increment).await
    }

    pub async fn hkeys_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hkeys_cmd(key).await
    }

    pub async fn hlen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hlen_cmd(key).await
    }

    pub async fn hmget_cmd(&self, key: &str, fields: Vec<String>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hmget_cmd(key, fields).await
    }

    pub async fn hmset_cmd(&self, key: &str, field_values: Vec<(String, String)>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hmset_cmd(key, field_values).await
    }

    pub async fn hsetnx_cmd(&self, key: &str, field: String, value: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hsetnx_cmd(key, field, value).await
    }

    pub async fn hvals_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hvals_cmd(key).await
    }

    // List Commands
    pub async fn lpush_cmd(&self, key: String, values: Vec<String>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.lpush_cmd(key, values).await
    }

    pub async fn rpush_cmd(&self, key: String, values: Vec<String>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.rpush_cmd(key, values).await
    }

    pub async fn lpop_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.lpop_cmd(key).await
    }

    pub async fn rpop_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.rpop_cmd(key).await
    }

    pub async fn llen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.llen_cmd(key).await
    }

    pub async fn lrange_cmd(&self, key: &str, start: isize, end: isize) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.lrange_cmd(key, start, end).await
    }

    pub async fn lindex_cmd(&self, key: &str, index: isize) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.lindex_cmd(key, index).await
    }

    pub async fn linsert_cmd(&self, key: &str, before_after: InsertOption, pivot: String, value: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.linsert_cmd(key, before_after, pivot, value).await
    }

    pub async fn lset_cmd(&self, key: &str, index: isize, value: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.lset_cmd(key, index, value).await
    }

    pub async fn ltrim_cmd(&self, key: &str, start: isize, end: isize) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.ltrim_cmd(key, start, end).await
    }

    pub async fn lrem_cmd(&self, key: &str, count: i64, value: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.lrem_cmd(key, count, value).await
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
            let shard = self.get_shard(first_key);
            shard.blpop_cmd(keys, timeout).await
        } else {
            Ok(DataType::Nil) // No keys provided, return Nil immediately
        }
    }

    pub async fn brpop_cmd(&self, keys: Vec<String>, timeout: f64) -> Result<DataType, CacheError> {
         if let Some(first_key) = keys.first() {
            let shard = self.get_shard(first_key);
            shard.brpop_cmd(keys, timeout).await
        } else {
            Ok(DataType::Nil) // No keys provided, return Nil immediately
        }
    }

    // --- JSON Commands ---
    pub async fn json_set_cmd(&self, key: String, path: String, value: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_set_cmd(key, path, value).await
    }

    pub async fn json_get_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_get_cmd(key, path).await
    }

    pub async fn json_del_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_del_cmd(key, path).await
    }

    pub async fn json_type_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_type_cmd(key, path).await
    }

    pub async fn json_numincrby_cmd(&self, key: String, path: String, increment: f64) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_numincrby_cmd(key, path, increment).await
    }

    pub async fn json_strappend_cmd(&self, key: String, path: String, value: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_strappend_cmd(key, path, value).await
    }

    pub async fn json_arrappend_cmd(&self, key: String, path: String, values: Vec<String>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_arrappend_cmd(key, path, values).await
    }

    pub async fn json_objset_cmd(&self, key: String, path: String, key_to_set: String, value: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_objset_cmd(key, path, key_to_set, value).await
    }

    pub async fn json_objkeys_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_objkeys_cmd(key, path).await
    }

    pub async fn json_objlen_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_objlen_cmd(key, path).await
    }

    pub async fn json_arrindex_cmd(&self, key: String, path: String, value: String, range: Option<(isize, isize)>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_arrindex_cmd(key, path, value, range).await
    }

    pub async fn json_arrinsert_cmd(&self, key: String, path: String, index: isize, values: Vec<String>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_arrinsert_cmd(key, path, index, values).await
    }

    pub async fn json_arrlen_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_arrlen_cmd(key, path).await
    }

    pub async fn json_arrpop_cmd(&self, key: String, path: String, index: Option<isize>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_arrpop_cmd(key, path, index).await
    }

    pub async fn json_arrtrim_cmd(&self, key: String, path: String, start: isize, stop: isize) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_arrtrim_cmd(key, path, start, stop).await
    }

    // --- Graph Commands ---
    pub async fn graph_create_node_cmd(&self, key: String, node_id: String, properties: Vec<(String, String)>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.graph_create_node_cmd(key, node_id, properties).await
    }

    pub async fn graph_get_node_cmd(&self, key: String, node_id: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.graph_get_node_cmd(key, node_id).await
    }

    pub async fn graph_delete_node_cmd(&self, key: String, node_id: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.graph_delete_node_cmd(key, node_id).await
    }

    pub fn metrics(&self) -> String {
        self.metrics.report()
    }
}