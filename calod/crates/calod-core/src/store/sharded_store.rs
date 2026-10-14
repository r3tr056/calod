use crate::store::command_options::{InsertOption, SetExpireOption, SetOption};
use rand::seq::SliceRandom;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;
use tracing::{error, info};
use twox_hash::XxHash64;

use super::calod_data::DataType;
use super::calod_shard::CalodShard;
use super::config::CalodConfig;
use super::directory::Directory;
use super::error::CacheError;
use super::hll::Hll;
use super::metrics::CalodMetrics;

type ShardIndex = usize;

pub struct ShardedStore {
    shards: Vec<Arc<CalodShard>>,
    shard_mask: u64,
    metrics: Arc<CalodMetrics>,
    config: Arc<CalodConfig>,
    /// Global directory (local view) mapping key hash -> physical metadata
    directory: Arc<Directory>,
}

impl ShardedStore {
    /// Create a new sharded store with the given configuration
    pub fn new(config: CalodConfig) -> Self {
        let num_shards = config.shards.next_power_of_two();
        let shard_capacity = config.shard_capacity;
        let metrics = Arc::new(CalodMetrics::default());

        info!(
            "Creating ShardedStore with {} shards, {} capacity each",
            num_shards, shard_capacity
        );

        let mut shards = Vec::with_capacity(num_shards);
        for i in 0..num_shards {
            shards.push(Arc::new(CalodShard::new(
                shard_capacity,
                metrics.clone(),
                i,
            )));
        }

        Self {
            shards,
            metrics,
            shard_mask: (num_shards - 1) as u64,
            config: Arc::new(config),
            directory: Directory::new(),
        }
    }

    /// Get the shard index asn hash for a given key
    #[inline]
    pub fn get_shard_index_and_hash<K: Hash + ?Sized>(&self, key: &K) -> (ShardIndex, u64) {
        // Fixed-seed hash: shard routing must be identical across processes.
        // Snapshots are stored as per-shard files (`shard_N.bin`); a
        // per-process-random hash (e.g. `AHasher::default()`) would route
        // keys to different shards after a restart and silently lose data.
        let mut hasher = XxHash64::with_seed(0);
        key.hash(&mut hasher);
        let hash = hasher.finish();
        ((hash & self.shard_mask) as usize, hash)
    }

    /// Get Shard index for a given key
    pub fn get_shard_index<K: Hash + ?Sized>(&self, key: &K) -> ShardIndex {
        self.get_shard_index_and_hash(key).0
    }

    /// Get a reference to the shard that contains the given key
    pub fn get_shard(&self, key: &str) -> Arc<CalodShard> {
        let index = self.get_shard_index(key);
        self.shards[index].clone()
    }

    /// Borrow the live server configuration (backs `CONFIG GET` and `INFO`).
    #[inline]
    pub fn config(&self) -> &CalodConfig {
        &self.config
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
            let future = tokio::spawn(async move { shard.keys(&pattern).await });
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

    pub async fn pttl(&self, key: &str) -> Result<Option<i64>, CacheError> {
        let shard = self.get_shard(key);
        shard.pttl(key).await
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
        let list_data: Vec<Vec<u8>> = keys.into_iter().map(String::into_bytes).collect();
        Ok(DataType::List(list_data))
    }

    pub async fn randomkey_cmd(&self) -> Result<DataType, CacheError> {
        let mut shard_indexes: Vec<usize> = (0..self.shards.len()).collect();
        {
            let mut rng = rand::rng();
            shard_indexes.shuffle(&mut rng);
        }

        for shard_idx in shard_indexes {
            if let Some(key) = self.shards[shard_idx].random_key().await {
                return Ok(DataType::string_from(key));
            }
        }

        Ok(DataType::Nil)
    }

    pub async fn touch_cmd(&self, keys: &[String]) -> Result<DataType, CacheError> {
        let mut touched = 0_i64;
        for key in keys {
            let shard = self.get_shard(key);
            if shard.touch_cmd(key).await {
                touched += 1;
            }
        }
        Ok(DataType::Integer(touched))
    }

    pub async fn rename_cmd(
        &self,
        key: &str,
        new_key: &str,
        nx: bool,
    ) -> Result<DataType, CacheError> {
        let old_idx = self.get_shard_index(key);
        let new_idx = self.get_shard_index(new_key);

        if key == new_key {
            return Err(CacheError::InvalidCommandArguments(
                "ERR source and destination objects are the same".to_string(),
            ));
        }

        let renamed = if old_idx == new_idx {
            let shard = self.shards[old_idx].clone();
            shard.rename_key(key, new_key, nx).await?
        } else if nx && self.shards[new_idx].touch_cmd(new_key).await {
            false
        } else {
            let Some(entry) = self.shards[old_idx].take_entry(key).await else {
                return Err(CacheError::KeyNotFound(key.to_string()));
            };
            let _ = self.shards[new_idx]
                .put_entry(new_key.to_string(), entry)
                .await;
            true
        };

        if nx {
            Ok(DataType::Integer(if renamed { 1 } else { 0 }))
        } else {
            Ok(DataType::Status("OK".to_string()))
        }
    }

    pub async fn copy_cmd(
        &self,
        source: &str,
        destination: &str,
        destination_db: Option<u32>,
        replace: bool,
    ) -> Result<DataType, CacheError> {
        if destination_db.unwrap_or(0) != 0 {
            return Err(CacheError::InvalidCommandArguments(
                "ERR DB index is out of range".to_string(),
            ));
        }

        let source_value = match self.get(source).await {
            Ok(v) => v,
            Err(CacheError::KeyNotFound(_)) | Err(CacheError::KeyExpired(_)) => {
                return Ok(DataType::Integer(0));
            }
            Err(e) => return Err(e),
        };

        if !replace && self.exists(destination).await {
            return Ok(DataType::Integer(0));
        }

        let source_shard = self.get_shard(source);
        let source_ttl = source_shard.pttl(source).await.ok().flatten();

        let destination_shard = self.get_shard(destination);
        let ttl = source_ttl
            .and_then(|ms| if ms > 0 { Some(ms as u64) } else { None })
            .map(std::time::Duration::from_millis);

        destination_shard
            .set(destination.to_string(), source_value, ttl)
            .await;

        Ok(DataType::Integer(1))
    }

    pub async fn move_cmd(&self, key: &str, db: u32) -> Result<DataType, CacheError> {
        if db != 0 {
            return Ok(DataType::Integer(0));
        }
        if self.exists(key).await {
            Ok(DataType::Integer(1))
        } else {
            Ok(DataType::Integer(0))
        }
    }

    pub async fn exists_cmd(&self, keys: &[String]) -> Result<DataType, CacheError> {
        let mut count = 0_i64;
        for key in keys {
            if self.exists(key).await {
                count += 1;
            }
        }
        Ok(DataType::Integer(count))
    }

    pub async fn expire_cmd(&self, key: &str, seconds: u64) -> Result<DataType, CacheError> {
        match self.expire(key, seconds).await {
            Ok(expired) => Ok(DataType::Integer(if expired { 1 } else { 0 })),
            Err(CacheError::KeyNotFound(_)) | Err(CacheError::KeyExpired(_)) => {
                Ok(DataType::Integer(0))
            }
            Err(e) => Err(e),
        }
    }

    pub async fn pexpire_cmd(&self, key: &str, milliseconds: u64) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        match shard.pexpire(key, milliseconds).await {
            Ok(expired) => Ok(DataType::Integer(if expired { 1 } else { 0 })),
            Err(CacheError::KeyNotFound(_)) | Err(CacheError::KeyExpired(_)) => {
                Ok(DataType::Integer(0))
            }
            Err(e) => Err(e),
        }
    }

    pub async fn expireat_cmd(&self, key: &str, when_secs: u64) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        let expired = shard.expireat(key, when_secs).await?;
        Ok(DataType::Integer(if expired { 1 } else { 0 }))
    }

    pub async fn pexpireat_cmd(&self, key: &str, when_ms: u64) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        let expired = shard.pexpireat(key, when_ms).await?;
        Ok(DataType::Integer(if expired { 1 } else { 0 }))
    }

    pub async fn ttl_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        match self.ttl(key).await {
            Ok(Some(ttl)) => Ok(DataType::Integer(ttl)),
            Ok(None) => Ok(DataType::Integer(-1)),
            Err(CacheError::KeyNotFound(_)) | Err(CacheError::KeyExpired(_)) => {
                Ok(DataType::Integer(-2))
            }
            Err(e) => Err(e),
        }
    }

    pub async fn pttl_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        match self.pttl(key).await {
            Ok(Some(ttl)) => Ok(DataType::Integer(ttl)),
            Ok(None) => Ok(DataType::Integer(-1)),
            Err(CacheError::KeyNotFound(_)) | Err(CacheError::KeyExpired(_)) => {
                Ok(DataType::Integer(-2))
            }
            Err(e) => Err(e),
        }
    }

    pub async fn expiretime_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        match shard.expiretime(key).await {
            Ok(Some(v)) => Ok(DataType::Integer(v)),
            Ok(None) => Ok(DataType::Integer(-1)),
            Err(CacheError::KeyNotFound(_)) | Err(CacheError::KeyExpired(_)) => {
                Ok(DataType::Integer(-2))
            }
            Err(e) => Err(e),
        }
    }

    pub async fn pexpiretime_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        match shard.pexpiretime(key).await {
            Ok(Some(v)) => Ok(DataType::Integer(v)),
            Ok(None) => Ok(DataType::Integer(-1)),
            Err(CacheError::KeyNotFound(_)) | Err(CacheError::KeyExpired(_)) => {
                Ok(DataType::Integer(-2))
            }
            Err(e) => Err(e),
        }
    }

    pub async fn dbsize_cmd(&self) -> Result<DataType, CacheError> {
        let mut total = 0_i64;
        for shard in &self.shards {
            total += shard.current_size() as i64;
        }
        Ok(DataType::Integer(total))
    }

    pub async fn flushdb_cmd(&self) -> Result<DataType, CacheError> {
        for shard in &self.shards {
            shard.clear_all().await;
        }
        Ok(DataType::Status("OK".to_string()))
    }

    pub async fn flushall_cmd(&self) -> Result<DataType, CacheError> {
        self.flushdb_cmd().await
    }

    /// Remove and return a key's whole entry (preserving TTL) — cross-DB `MOVE`.
    pub async fn take_entry(&self, key: &str) -> Option<super::calod_data::CacheEntry> {
        self.get_shard(key).take_entry(key).await
    }

    /// Non-destructively clone a key's entry (preserving TTL) — cross-DB `COPY`.
    pub async fn get_entry(&self, key: &str) -> Option<super::calod_data::CacheEntry> {
        self.get_shard(key).get_entry(key).await
    }

    /// Insert a whole entry under `key` (used to land a moved/copied entry).
    pub async fn put_entry(&self, key: String, entry: super::calod_data::CacheEntry) {
        let _ = self.get_shard(&key).put_entry(key, entry).await;
    }

    pub async fn scan_cmd(
        &self,
        cursor: u64,
        pattern: Option<String>,
        count: Option<usize>,
    ) -> Result<DataType, CacheError> {
        let all_keys = self.keys(pattern.as_deref().unwrap_or("*")).await;
        let step = count.unwrap_or(10).max(1);
        let start = cursor as usize;

        if start >= all_keys.len() {
            return Ok(DataType::Array(vec![
                DataType::string_from("0"),
                DataType::Array(Vec::new()),
            ]));
        }

        let end = (start + step).min(all_keys.len());
        let next_cursor = if end >= all_keys.len() { 0 } else { end as u64 };

        let chunk = all_keys[start..end]
            .iter()
            .cloned()
            .map(DataType::string_from)
            .collect::<Vec<DataType>>();

        Ok(DataType::Array(vec![
            DataType::string_from(next_cursor.to_string()),
            DataType::Array(chunk),
        ]))
    }

    pub async fn persist_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let persisted = self.persist(key).await?;
        Ok(DataType::Integer(if persisted { 1 } else { 0 }))
    }

    pub async fn object_encoding_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        match shard.get(key).await {
            Ok(data) => {
                let encoding = match data {
                    DataType::String(_) | DataType::Status(_) => "raw",
                    DataType::Integer(_) => "int",
                    DataType::List(_) => "quicklist",
                    DataType::Hash(_) => "hashtable",
                    DataType::Set(_) => "hashtable",
                    DataType::Document(_) => "json",
                    DataType::SortedSet(_) => "skiplist",
                    DataType::Stream(_) => "stream",
                    DataType::Double(_) => "int",
                    DataType::Object { .. } => "object",
                    DataType::Array(_) => "array",
                    DataType::Nil | DataType::NilArray => "none",
                };
                Ok(DataType::string_from(encoding.to_string()))
            }
            Err(CacheError::KeyNotFound(_)) | Err(CacheError::KeyExpired(_)) => Ok(DataType::Nil),
            Err(e) => Err(e),
        }
    }

    pub async fn object_refcount_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        if self.exists(key).await {
            Ok(DataType::Integer(1))
        } else {
            Ok(DataType::Nil)
        }
    }

    pub async fn object_idletime_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        if self.exists(key).await {
            Ok(DataType::Integer(0))
        } else {
            Ok(DataType::Nil)
        }
    }

    pub async fn wait_cmd(
        &self,
        _numreplicas: u64,
        _timeout_ms: u64,
    ) -> Result<DataType, CacheError> {
        Ok(DataType::Integer(0))
    }

    pub async fn del_cmd(&self, keys: &[String]) -> Result<DataType, CacheError> {
        let mut deleted_count = 0_i64;
        for key in keys {
            if self.delete(key).await {
                deleted_count += 1;
            }
        }
        Ok(DataType::Integer(deleted_count))
    }

    pub async fn unlink_cmd(&self, keys: &[String]) -> Result<DataType, CacheError> {
        self.del_cmd(keys).await
    }

    /// Read the raw string bytes stored at `key`, treating missing/expired keys
    /// and Nil as empty. Returns the WRONGTYPE-style mismatch error for any
    /// non-string value.
    async fn read_string_bytes(&self, key: &str) -> Result<Vec<u8>, CacheError> {
        match self.get(key).await {
            Ok(DataType::String(bytes)) => Ok(bytes),
            Ok(DataType::Nil) => Ok(Vec::new()),
            Err(CacheError::KeyNotFound(_)) | Err(CacheError::KeyExpired(_)) => Ok(Vec::new()),
            Ok(other) => Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "string".to_string(),
                other.data_type(),
            )),
            Err(e) => Err(e),
        }
    }

    /// Command: BITOP – Perform a bitwise AND/OR/XOR/NOT across `keys`, storing
    /// the result into `dest`. Returns the length in bytes of the stored result.
    pub async fn bitop_cmd(
        &self,
        op: &str,
        dest: &str,
        keys: &[String],
    ) -> Result<DataType, CacheError> {
        let result: Vec<u8> = if op == "NOT" {
            // NOT operates on exactly one source key.
            if keys.len() != 1 {
                return Err(CacheError::InvalidCommandArguments(
                    "ERR BITOP NOT must be called with a single source key.".into(),
                ));
            }
            let src = self.read_string_bytes(&keys[0]).await?;
            src.iter().map(|b| !b).collect()
        } else {
            // Read every source operand first (missing keys = empty bytes).
            let mut operands: Vec<Vec<u8>> = Vec::with_capacity(keys.len());
            for key in keys {
                operands.push(self.read_string_bytes(key).await?);
            }

            let max_len = operands.iter().map(|o| o.len()).max().unwrap_or(0);
            let mut acc = vec![0u8; max_len];

            for (i, operand) in operands.iter().enumerate() {
                for (pos, acc_byte) in acc.iter_mut().enumerate() {
                    let byte = operand.get(pos).copied().unwrap_or(0);
                    if i == 0 {
                        *acc_byte = byte;
                    } else {
                        match op {
                            "AND" => *acc_byte &= byte,
                            "OR" => *acc_byte |= byte,
                            "XOR" => *acc_byte ^= byte,
                            _ => {
                                return Err(CacheError::InvalidCommandArguments(
                                    "ERR syntax error".into(),
                                ));
                            }
                        }
                    }
                }
            }
            acc
        };

        let result_len = result.len();
        if result_len == 0 {
            // An empty result deletes the destination key.
            let _ = self.delete(dest).await;
        } else {
            let shard = self.get_shard(dest);
            shard
                .set(dest.to_string(), DataType::String(result), None)
                .await;
        }

        Ok(DataType::Integer(result_len as i64))
    }

    /// Command: PFCOUNT – Estimate the combined cardinality of the HyperLogLogs
    /// stored at `keys`. Missing keys are skipped; a present non-HLL string is an
    /// error.
    pub async fn pfcount_cmd(&self, keys: &[String]) -> Result<DataType, CacheError> {
        let mut merged: Option<Hll> = None;

        for key in keys {
            let bytes = self.read_string_bytes(key).await?;
            if bytes.is_empty() {
                // Missing key (or empty string) contributes nothing.
                continue;
            }
            let Some(hll) = Hll::from_bytes(&bytes) else {
                return Err(CacheError::InvalidCommandArguments(
                    "WRONGTYPE Key is not a valid HyperLogLog string value.".into(),
                ));
            };
            match &mut merged {
                Some(acc) => acc.merge(&hll),
                None => merged = Some(hll),
            }
        }

        let count = merged.map(|h| h.count()).unwrap_or(0);
        Ok(DataType::Integer(count as i64))
    }

    /// Command: PFMERGE – Merge the HyperLogLogs at `dest` and `sources` into a
    /// single sketch stored at `dest`. Missing keys are treated as empty HLLs; a
    /// present non-HLL string is an error.
    pub async fn pfmerge_cmd(
        &self,
        dest: &str,
        sources: &[String],
    ) -> Result<DataType, CacheError> {
        let mut merged = Hll::new();

        // Fold dest first (if present), then every source.
        let dest_bytes = self.read_string_bytes(dest).await?;
        if !dest_bytes.is_empty() {
            let Some(hll) = Hll::from_bytes(&dest_bytes) else {
                return Err(CacheError::InvalidCommandArguments(
                    "WRONGTYPE Key is not a valid HyperLogLog string value.".into(),
                ));
            };
            merged.merge(&hll);
        }

        for key in sources {
            let bytes = self.read_string_bytes(key).await?;
            if bytes.is_empty() {
                continue;
            }
            let Some(hll) = Hll::from_bytes(&bytes) else {
                return Err(CacheError::InvalidCommandArguments(
                    "WRONGTYPE Key is not a valid HyperLogLog string value.".into(),
                ));
            };
            merged.merge(&hll);
        }

        let shard = self.get_shard(dest);
        shard
            .set(dest.to_string(), DataType::String(merged.to_bytes()), None)
            .await;

        Ok(DataType::Status("OK".into()))
    }

    // strings/numbers commands

    pub async fn get_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let result = self.get(key).await;
        match result {
            Ok(DataType::String(_)) | Ok(DataType::Nil) => result, // Only return Strings or Nil for GET
            Ok(other_type) => Err(CacheError::DataTypeMismatch(
                key.to_string(),
                "string".to_string(),
                other_type.data_type(),
            )),
            Err(CacheError::KeyNotFound(_)) | Err(CacheError::KeyExpired(_)) => Ok(DataType::Nil),
            Err(e) => Err(e),
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

    pub async fn set_cmd(
        &self,
        key: String,
        value: Vec<u8>,
        expire_option: Option<SetExpireOption>,
        set_option: Option<SetOption>,
    ) -> Result<bool, CacheError> {
        let shard = self.get_shard(&key);
        // We capture length before move
        let val_len = value.len();
        let changed = shard
            .set_cmd(
                key.clone(),
                DataType::String(value),
                expire_option,
                set_option,
            )
            .await?;
        if !changed {
            return Ok(false);
        }
        // Publish directory entry (best effort, ignore collisions for now)
        let key_hash = Directory::hash_key(&key);
        let shard_id = shard.id() as u32;
        self.directory.publish(key_hash, shard_id, val_len);
        Ok(true)
    }

    pub async fn setnx_cmd(&self, key: String, value: Vec<u8>) -> Result<DataType, CacheError> {
        if self.exists(&key).await {
            return Ok(DataType::Integer(0));
        }
        let inserted = self.set_cmd(key, value, None, Some(SetOption::NX)).await?;
        Ok(DataType::Integer(if inserted { 1 } else { 0 }))
    }

    pub async fn append_cmd(&self, key: &str, value: Vec<u8>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.append_cmd(key, value).await
    }

    pub async fn strlen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.strlen_cmd(key).await
    }

    pub async fn getrange_cmd(
        &self,
        key: &str,
        start: isize,
        end: isize,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.getrange_cmd(key, start, end).await
    }

    pub async fn setrange_cmd(
        &self,
        key: &str,
        offset: usize,
        value: Vec<u8>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.setrange_cmd(key, offset, value).await
    }

    pub async fn getset_cmd(&self, key: &str, value: Vec<u8>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.getset_cmd(key, value).await
    }

    // Bitmap commands (single-key passthroughs)

    pub async fn setbit_cmd(
        &self,
        key: &str,
        offset: u64,
        value: u8,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.setbit_cmd(key, offset, value).await
    }

    pub async fn getbit_cmd(&self, key: &str, offset: u64) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.getbit_cmd(key, offset).await
    }

    pub async fn bitcount_cmd(
        &self,
        key: &str,
        range: Option<(i64, i64)>,
        bit_unit: bool,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.bitcount_cmd(key, range, bit_unit).await
    }

    pub async fn bitpos_cmd(
        &self,
        key: &str,
        bit: u8,
        start: Option<i64>,
        end: Option<i64>,
        bit_unit: bool,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.bitpos_cmd(key, bit, start, end, bit_unit).await
    }

    // HyperLogLog command (single-key passthrough)

    pub async fn pfadd_cmd(&self, key: &str, elements: &[Vec<u8>]) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.pfadd_cmd(key, elements).await
    }

    // Stream commands (single-key passthroughs)

    pub async fn xadd_cmd(
        &self,
        key: &str,
        id: &str,
        nomkstream: bool,
        maxlen: Option<usize>,
        fields: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xadd_cmd(key, id, nomkstream, maxlen, fields).await
    }

    pub async fn xlen_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xlen_cmd(key).await
    }

    pub async fn xrange_cmd(
        &self,
        key: &str,
        start: &str,
        end: &str,
        count: Option<usize>,
        rev: bool,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xrange_cmd(key, start, end, count, rev).await
    }

    pub async fn xdel_cmd(&self, key: &str, ids: &[String]) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xdel_cmd(key, ids).await
    }

    pub async fn xtrim_cmd(&self, key: &str, maxlen: usize) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xtrim_cmd(key, maxlen).await
    }

    /// Command: XREAD – Non-blocking read of new entries from one or more
    /// streams. For each `(key, id_token)` the per-shard `xread_one` resolves
    /// the "after" ID and collects entries strictly greater than it. Streams
    /// that yield no entries (including missing keys) contribute nothing; if no
    /// stream produces entries the reply is Nil.
    pub async fn xread_cmd(
        &self,
        streams: &[(String, String)],
        count: Option<usize>,
    ) -> Result<DataType, CacheError> {
        let mut replies: Vec<DataType> = Vec::with_capacity(streams.len());

        for (key, id_token) in streams {
            let shard = self.get_shard(key);
            if let Some(reply) = shard.xread_one(key, id_token, count).await? {
                replies.push(reply);
            }
        }

        if replies.is_empty() {
            Ok(DataType::Nil)
        } else {
            Ok(DataType::Array(replies))
        }
    }

    // Stream consumer-group commands (single-key passthroughs)

    pub async fn xgroup_create_cmd(
        &self,
        key: &str,
        group: &str,
        id: &str,
        mkstream: bool,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xgroup_create_cmd(key, group, id, mkstream).await
    }

    pub async fn xgroup_setid_cmd(
        &self,
        key: &str,
        group: &str,
        id: &str,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xgroup_setid_cmd(key, group, id).await
    }

    pub async fn xgroup_destroy_cmd(&self, key: &str, group: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xgroup_destroy_cmd(key, group).await
    }

    pub async fn xgroup_createconsumer_cmd(
        &self,
        key: &str,
        group: &str,
        consumer: &str,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xgroup_createconsumer_cmd(key, group, consumer).await
    }

    pub async fn xgroup_delconsumer_cmd(
        &self,
        key: &str,
        group: &str,
        consumer: &str,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xgroup_delconsumer_cmd(key, group, consumer).await
    }

    pub async fn xack_cmd(
        &self,
        key: &str,
        group: &str,
        ids: &[String],
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xack_cmd(key, group, ids).await
    }

    pub async fn xpending_summary_cmd(
        &self,
        key: &str,
        group: &str,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xpending_summary_cmd(key, group).await
    }

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
        let shard = self.get_shard(key);
        shard
            .xpending_extended_cmd(key, group, idle, start, end, count, consumer)
            .await
    }

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
        let shard = self.get_shard(key);
        shard
            .xclaim_cmd(
                key, group, consumer, min_idle, ids, idle, time, retrycount, force, justid,
            )
            .await
    }

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
        let shard = self.get_shard(key);
        shard
            .xautoclaim_cmd(key, group, consumer, min_idle, start, count, justid)
            .await
    }

    pub async fn xinfo_stream_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xinfo_stream_cmd(key).await
    }

    pub async fn xinfo_groups_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xinfo_groups_cmd(key).await
    }

    pub async fn xinfo_consumers_cmd(
        &self,
        key: &str,
        group: &str,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xinfo_consumers_cmd(key, group).await
    }

    pub async fn xsetid_cmd(
        &self,
        key: &str,
        id: &str,
        entries_added: Option<u64>,
        max_deleted: Option<&str>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.xsetid_cmd(key, id, entries_added, max_deleted).await
    }

    /// Command: XREADGROUP – Group-aware read of one or more streams. For each
    /// `(key, id_token)` the per-shard `xreadgroup_one` reads either new (`>`)
    /// or pending (explicit-id) entries; a missing key/group is a `NOGROUP`
    /// error. Empty `>` results are omitted; if no element is produced the reply
    /// is Nil.
    pub async fn xreadgroup_cmd(
        &self,
        group: &str,
        consumer: &str,
        count: Option<usize>,
        noack: bool,
        streams: &[(String, String)],
    ) -> Result<DataType, CacheError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let mut elems: Vec<DataType> = Vec::with_capacity(streams.len());

        for (key, id_token) in streams {
            let shard = self.get_shard(key);
            if let Some(elem) = shard
                .xreadgroup_one(key, group, consumer, id_token, count, noack, now_ms)
                .await?
            {
                elems.push(elem);
            }
        }

        if elems.is_empty() {
            Ok(DataType::Nil)
        } else {
            Ok(DataType::Array(elems))
        }
    }

    pub async fn getdel_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        match self.get_cmd(key).await {
            Ok(DataType::Nil) => Ok(DataType::Nil),
            Ok(current) => {
                let _ = self.delete(key).await;
                Ok(current)
            }
            Err(CacheError::KeyNotFound(_)) | Err(CacheError::KeyExpired(_)) => Ok(DataType::Nil),
            Err(e) => Err(e),
        }
    }

    pub async fn getex_cmd(
        &self,
        key: &str,
        option: Option<crate::store::command_options::GetExOption>,
    ) -> Result<DataType, CacheError> {
        let current = self.get_cmd(key).await?;
        if matches!(current, DataType::Nil) {
            return Ok(DataType::Nil);
        }

        match option {
            None => {}
            Some(crate::store::command_options::GetExOption::Persist) => {
                let _ = self.persist_cmd(key).await?;
            }
            Some(crate::store::command_options::GetExOption::EX(seconds)) => {
                let _ = self.expire_cmd(key, seconds).await?;
            }
            Some(crate::store::command_options::GetExOption::PX(ms)) => {
                let _ = self.pexpire_cmd(key, ms).await?;
            }
            Some(crate::store::command_options::GetExOption::EXAT(ts)) => {
                let _ = self.expireat_cmd(key, ts).await?;
            }
            Some(crate::store::command_options::GetExOption::PXAT(ts_ms)) => {
                let _ = self.pexpireat_cmd(key, ts_ms).await?;
            }
        }

        Ok(current)
    }

    pub async fn mget_cmd(&self, keys: &Vec<String>) -> Result<DataType, CacheError> {
        let mut results = Vec::with_capacity(keys.len());
        for key in keys {
            let shard = self.get_shard(key);
            results.push(shard.get(key).await);
        }

        let resp_values = results
            .into_iter()
            .map(|res| match res {
                Ok(DataType::String(s)) => DataType::String(s),
                Ok(DataType::Nil)
                | Err(CacheError::KeyNotFound(_))
                | Err(CacheError::KeyExpired(_)) => DataType::Nil,
                Err(e) => {
                    error!("Error during MGET: {:?}", e);
                    DataType::string_from(format!("ERR {:?}", e))
                }
                Ok(other) => {
                    error!("Unexpected DataType in MGET: {:?}", other);
                    DataType::string_from(format!("ERR unexpected type {}", other.data_type()))
                }
            })
            .collect::<Vec<DataType>>();

        Ok(DataType::Array(resp_values))
    }

    pub async fn mset_cmd(&self, key_values: Vec<(String, Vec<u8>)>) -> Result<(), CacheError> {
        for (key, value) in key_values {
            let shard = self.get_shard(&key);
            let val_len = value.len();
            shard
                .set_cmd(key.clone(), DataType::String(value), None, None)
                .await?; // No expiry/option for MSET in this example
            let key_hash = Directory::hash_key(&key);
            self.directory.publish(key_hash, shard.id() as u32, val_len);
        }
        Ok(())
    }

    pub async fn msetnx_cmd(
        &self,
        key_values: Vec<(String, Vec<u8>)>,
    ) -> Result<DataType, CacheError> {
        for (key, _) in &key_values {
            if self.exists(key).await {
                return Ok(DataType::Integer(0));
            }
        }

        for (key, value) in key_values {
            let _ = self.set_cmd(key, value, None, Some(SetOption::NX)).await?;
        }

        Ok(DataType::Integer(1))
    }

    pub async fn sadd_cmd(&self, key: &str, members: Vec<Vec<u8>>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.sadd_cmd(key, members).await
    }

    pub async fn srem_cmd(&self, key: &str, members: Vec<Vec<u8>>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.srem_cmd(key, members).await
    }

    pub async fn smembers_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.smembers_cmd(key).await
    }

    pub async fn sismember_cmd(&self, key: &str, member: &[u8]) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.sismember_cmd(key, member).await
    }

    pub async fn scard_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.scard_cmd(key).await
    }

    pub async fn spop_cmd(&self, key: &str, count: Option<usize>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.spop_cmd(key, count).await
    }

    pub async fn srandmember_cmd(
        &self,
        key: &str,
        count: Option<isize>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.srandmember_cmd(key, count).await
    }

    // Sorted Set (ZSET) commands
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
        let shard = self.get_shard(key);
        shard.zadd_cmd(key, nx, xx, gt, lt, ch, incr, members).await
    }

    pub async fn zscore_cmd(&self, key: &str, member: &[u8]) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zscore_cmd(key, member).await
    }

    pub async fn zmscore_cmd(
        &self,
        key: &str,
        members: &[Vec<u8>],
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zmscore_cmd(key, members).await
    }

    pub async fn zcard_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zcard_cmd(key).await
    }

    pub async fn zincrby_cmd(
        &self,
        key: &str,
        increment: f64,
        member: Vec<u8>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zincrby_cmd(key, increment, member).await
    }

    pub async fn zrem_cmd(&self, key: &str, members: &[Vec<u8>]) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zrem_cmd(key, members).await
    }

    pub async fn zrank_cmd(
        &self,
        key: &str,
        member: &[u8],
        rev: bool,
        with_score: bool,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zrank_cmd(key, member, rev, with_score).await
    }

    pub async fn zrange_cmd(
        &self,
        key: &str,
        start: i64,
        stop: i64,
        rev: bool,
        with_scores: bool,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zrange_cmd(key, start, stop, rev, with_scores).await
    }

    pub async fn zrangebyscore_cmd(
        &self,
        key: &str,
        min: &str,
        max: &str,
        rev: bool,
        with_scores: bool,
        limit: Option<(i64, i64)>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard
            .zrangebyscore_cmd(key, min, max, rev, with_scores, limit)
            .await
    }

    pub async fn zrangebylex_cmd(
        &self,
        key: &str,
        min: &str,
        max: &str,
        rev: bool,
        limit: Option<(i64, i64)>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zrangebylex_cmd(key, min, max, rev, limit).await
    }

    pub async fn zcount_cmd(
        &self,
        key: &str,
        min: &str,
        max: &str,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zcount_cmd(key, min, max).await
    }

    pub async fn zlexcount_cmd(
        &self,
        key: &str,
        min: &str,
        max: &str,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zlexcount_cmd(key, min, max).await
    }

    pub async fn zpopmin_cmd(
        &self,
        key: &str,
        count: Option<usize>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zpopmin_cmd(key, count).await
    }

    pub async fn zpopmax_cmd(
        &self,
        key: &str,
        count: Option<usize>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zpopmax_cmd(key, count).await
    }

    pub async fn zremrangebyrank_cmd(
        &self,
        key: &str,
        start: i64,
        stop: i64,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zremrangebyrank_cmd(key, start, stop).await
    }

    pub async fn zremrangebyscore_cmd(
        &self,
        key: &str,
        min: &str,
        max: &str,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zremrangebyscore_cmd(key, min, max).await
    }

    pub async fn zremrangebylex_cmd(
        &self,
        key: &str,
        min: &str,
        max: &str,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zremrangebylex_cmd(key, min, max).await
    }

    pub async fn zrandmember_cmd(
        &self,
        key: &str,
        count: Option<i64>,
        with_scores: bool,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zrandmember_cmd(key, count, with_scores).await
    }

    pub async fn zscan_cmd(
        &self,
        key: &str,
        cursor: u64,
        pattern: Option<String>,
        count: Option<usize>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.zscan_cmd(key, cursor, pattern, count).await
    }

    // GEO commands
    #[allow(clippy::too_many_arguments)]
    pub async fn geoadd_cmd(
        &self,
        key: &str,
        nx: bool,
        xx: bool,
        ch: bool,
        items: Vec<(f64, f64, Vec<u8>)>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.geoadd_cmd(key, nx, xx, ch, items).await
    }

    pub async fn geopos_cmd(&self, key: &str, members: &[Vec<u8>]) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.geopos_cmd(key, members).await
    }

    pub async fn geodist_cmd(
        &self,
        key: &str,
        m1: &[u8],
        m2: &[u8],
        unit_div: f64,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.geodist_cmd(key, m1, m2, unit_div).await
    }

    pub async fn geohash_cmd(
        &self,
        key: &str,
        members: &[Vec<u8>],
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.geohash_cmd(key, members).await
    }

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
        let shard = self.get_shard(key);
        shard
            .geosearch_cmd(
                key,
                from_member,
                from_lonlat,
                radius_m,
                unit_div,
                with_coord,
                with_dist,
                with_hash,
                count,
                asc,
            )
            .await
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
    pub async fn hset_cmd(
        &self,
        key: &str,
        field_values: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hset_cmd(key, field_values).await
    }
    pub async fn hget_cmd(&self, key: &str, field: Vec<u8>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hget_cmd(key, field).await
    }

    pub async fn hdel_cmd(&self, key: &str, fields: Vec<Vec<u8>>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hdel_cmd(key, fields).await
    }

    pub async fn hexists_cmd(&self, key: &str, field: Vec<u8>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hexists_cmd(key, field).await
    }

    pub async fn hgetall_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hgetall_cmd(key).await
    }

    pub async fn hincrby_cmd(
        &self,
        key: &str,
        field: Vec<u8>,
        increment: i64,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hincrby_cmd(key, field, increment).await
    }

    pub async fn hincrbyfloat_cmd(
        &self,
        key: &str,
        key_field_increment: Vec<(Vec<u8>, f64)>,
    ) -> Result<DataType, CacheError> {
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

    pub async fn hmget_cmd(&self, key: &str, fields: Vec<Vec<u8>>) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hmget_cmd(key, fields).await
    }

    pub async fn hmset_cmd(
        &self,
        key: &str,
        field_values: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hmset_cmd(key, field_values).await
    }

    pub async fn hsetnx_cmd(
        &self,
        key: &str,
        field: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hsetnx_cmd(key, field, value).await
    }

    pub async fn hvals_cmd(&self, key: &str) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.hvals_cmd(key).await
    }

    // List Commands
    pub async fn lpush_cmd(
        &self,
        key: String,
        values: Vec<Vec<u8>>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.lpush_cmd(key, values).await
    }

    pub async fn rpush_cmd(
        &self,
        key: String,
        values: Vec<Vec<u8>>,
    ) -> Result<DataType, CacheError> {
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

    pub async fn lrange_cmd(
        &self,
        key: &str,
        start: isize,
        end: isize,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.lrange_cmd(key, start, end).await
    }

    pub async fn lindex_cmd(&self, key: &str, index: isize) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.lindex_cmd(key, index).await
    }

    pub async fn linsert_cmd(
        &self,
        key: &str,
        before_after: InsertOption,
        pivot: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.linsert_cmd(key, before_after, pivot, value).await
    }

    pub async fn lset_cmd(
        &self,
        key: &str,
        index: isize,
        value: Vec<u8>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.lset_cmd(key, index, value).await
    }

    pub async fn ltrim_cmd(
        &self,
        key: &str,
        start: isize,
        end: isize,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.ltrim_cmd(key, start, end).await
    }

    pub async fn lrem_cmd(
        &self,
        key: &str,
        count: i64,
        value: Vec<u8>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(key);
        shard.lrem_cmd(key, count, value).await
    }

    pub async fn rpoplpush_cmd(
        &self,
        source: &str,
        destination: &str,
    ) -> Result<DataType, CacheError> {
        match self.rpop_cmd(source).await? {
            DataType::String(popped) => {
                // List elements are binary-safe bytes; re-push verbatim.
                self.lpush_cmd(destination.to_string(), vec![popped.clone()])
                    .await?;
                Ok(DataType::String(popped))
            }
            DataType::Nil => Ok(DataType::Nil),
            other => Err(CacheError::DataTypeMismatch(
                source.to_string(),
                "List".to_string(),
                other.data_type(),
            )),
        }
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
    pub async fn json_set_cmd(
        &self,
        key: String,
        path: String,
        value: String,
    ) -> Result<DataType, CacheError> {
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

    pub async fn json_numincrby_cmd(
        &self,
        key: String,
        path: String,
        increment: f64,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_numincrby_cmd(key, path, increment).await
    }

    pub async fn json_strappend_cmd(
        &self,
        key: String,
        path: String,
        value: String,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_strappend_cmd(key, path, value).await
    }

    pub async fn json_arrappend_cmd(
        &self,
        key: String,
        path: String,
        values: Vec<String>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_arrappend_cmd(key, path, values).await
    }

    pub async fn json_objset_cmd(
        &self,
        key: String,
        path: String,
        key_to_set: String,
        value: String,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_objset_cmd(key, path, key_to_set, value).await
    }

    pub async fn json_objkeys_cmd(
        &self,
        key: String,
        path: String,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_objkeys_cmd(key, path).await
    }

    pub async fn json_objlen_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_objlen_cmd(key, path).await
    }

    pub async fn json_arrindex_cmd(
        &self,
        key: String,
        path: String,
        value: String,
        range: Option<(isize, isize)>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_arrindex_cmd(key, path, value, range).await
    }

    pub async fn json_arrinsert_cmd(
        &self,
        key: String,
        path: String,
        index: isize,
        values: Vec<String>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_arrinsert_cmd(key, path, index, values).await
    }

    pub async fn json_arrlen_cmd(&self, key: String, path: String) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_arrlen_cmd(key, path).await
    }

    pub async fn json_arrpop_cmd(
        &self,
        key: String,
        path: String,
        index: Option<isize>,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_arrpop_cmd(key, path, index).await
    }

    pub async fn json_arrtrim_cmd(
        &self,
        key: String,
        path: String,
        start: isize,
        stop: isize,
    ) -> Result<DataType, CacheError> {
        let shard = self.get_shard(&key);
        shard.json_arrtrim_cmd(key, path, start, stop).await
    }

    pub fn metrics(&self) -> String {
        self.metrics.report()
    }

    pub fn total_data_size(&self) -> u64 {
        self.metrics.total_data_size()
    }
}
