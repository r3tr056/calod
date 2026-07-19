use std::{
    net::IpAddr,
    path::Path,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc, Weak,
    },
};

use ahash::{AHashMap, RandomState};
use bytes::{BufMut, BytesMut};
use dashmap::DashMap;
use tokio::fs::{self, OpenOptions};
use tokio::sync::Mutex;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use super::{
    command::{ClientSubCommand, Command, ConfigSubCommand, ScriptSubCommand},
    connection::{Connection, TxnState},
    metrics::MetricsAggregator,
    protocol::{ConnCtx, ProtocolVersion},
    pubsub::PubSubRegistry,
    reply::datatype_to_value,
};
use crate::parser::{errors::RespError, parser::RESPParser, value::Value};
use crate::store::{
    aof::AofLogger,
    calod_data::DataType,
    config::{CalodConfig, PersistenceConfig},
    config_registry::{config_get, config_set, ConfigOverlay},
    error::{CacheError, PersistenceError},
    persistence::CachePersistence,
    sharded_store::ShardedStore,
    watch::WatchRegistry,
};

type ClientId = Uuid;

const PONG_RESPONSE: &[u8] = b"+PONG\r\n";
const OK_RESPONSE: &[u8] = b"+OK\r\n";

#[derive(Debug, PartialEq, Eq)]
enum CommandErrorLogLevel {
    Debug,
    Warn,
    Error,
}

pub struct ServedCalodStore {
    /// Database 0 — an alias of `dbs[0]`, kept for persistence/INFO helpers that
    /// operate on the default DB.
    store: Arc<ShardedStore>,
    /// All logical databases (Redis defaults to 16), indexed by the connection's
    /// `SELECT`ed `ConnCtx::db`. `dbs[0]` is the same `Arc` as `store`.
    dbs: Vec<Arc<ShardedStore>>,
    command_response_cache: AHashMap<&'static str, Vec<u8>>,
    aof: AofLogger,
    metrics: Arc<MetricsAggregator>,
    /// Runtime-mutable config overrides backing `CONFIG SET`/`GET`.
    config_overlay: Arc<ConfigOverlay>,
    /// Stable per-process run identifier reported by `INFO` (Redis `run_id`).
    run_id: String,
    /// Process start instant, for `INFO uptime_in_seconds`.
    start_time: std::time::Instant,
    /// Pub/Sub channel and pattern subscription registry (push path).
    pubsub: Arc<PubSubRegistry>,
    /// Per-watched-key version tracker backing WATCH/EXEC optimistic transactions.
    watch_registry: Arc<WatchRegistry>,
    /// Parsed `notify-keyspace-events` bitset. 0 = disabled (zero overhead on the
    /// hot path). Read lock-free in `execute_inner`.
    notify_flags: Arc<AtomicU32>,
    /// Cached Lua scripts keyed by lowercase SHA1 (backs `EVALSHA`/`SCRIPT`).
    scripts: Arc<DashMap<String, String>>,
    /// Unix time (seconds) of the last successful `SAVE`/`BGSAVE`, for `LASTSAVE`
    /// and `INFO persistence`.
    last_save: Arc<AtomicU64>,
}

fn parse_ip_port(ip_port_str: &str) -> Result<(IpAddr, u16), String> {
    let (ip_str, port_str) = ip_port_str
        .split_once(':')
        .ok_or_else(|| "Invalid ip:port fromat".to_string())?;

    let ip_addr: IpAddr = ip_str
        .parse()
        .map_err(|_| "Invalid IP address".to_string())?;
    let port: u16 = port_str
        .parse()
        .map_err(|_| "Invalid port number".to_string())?;

    Ok((ip_addr, port))
}

// Keyspace-notification class bits (a subset of Redis `notify-keyspace-events`).
const NK_GENERIC: u32 = 1 << 0; // 'g'
const NK_STRING: u32 = 1 << 1; // '$'
const NK_LIST: u32 = 1 << 2; // 'l'
const NK_SET: u32 = 1 << 3; // 's'
const NK_HASH: u32 = 1 << 4; // 'h'
const NK_ZSET: u32 = 1 << 5; // 'z'
const NK_STREAM: u32 = 1 << 6; // 't' / 'x'
const NK_ALL: u32 = NK_GENERIC | NK_STRING | NK_LIST | NK_SET | NK_HASH | NK_ZSET | NK_STREAM; // 'A'
const NOTIFY_KEYSPACE: u32 = 1 << 30; // 'K' -> __keyspace@db__:key channels
const NOTIFY_KEYEVENT: u32 = 1 << 31; // 'E' -> __keyevent@db__:event channels

/// Parse a Redis `notify-keyspace-events` flag string (e.g. "KEA", "Kl$") into a
/// bitset. Unsupported class letters are ignored.
fn parse_notify_flags(s: &str) -> u32 {
    let mut f = 0u32;
    for c in s.chars() {
        match c {
            'K' => f |= NOTIFY_KEYSPACE,
            'E' => f |= NOTIFY_KEYEVENT,
            'A' => f |= NK_ALL,
            'g' => f |= NK_GENERIC,
            '$' => f |= NK_STRING,
            'l' => f |= NK_LIST,
            's' => f |= NK_SET,
            'h' => f |= NK_HASH,
            'z' => f |= NK_ZSET,
            't' | 'x' => f |= NK_STREAM,
            _ => {}
        }
    }
    f
}

/// Meters per GEO distance unit (`m`/`km`/`mi`/`ft`); unknown defaults to meters.
fn geo_unit_div(unit: &str) -> f64 {
    match unit.to_ascii_lowercase().as_str() {
        "km" => 1000.0,
        "mi" => 1609.34,
        "ft" => 0.3048,
        _ => 1.0,
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "K", "M", "G"];
    let mut value = bytes as f64;
    let mut unit = 0usize;

    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{}B", bytes)
    } else {
        format!("{value:.2}{}", UNITS[unit])
    }
}

impl ServedCalodStore {
    pub async fn new() -> Self {
        Self::new_with_metrics(Arc::new(MetricsAggregator::new())).await
    }

    pub async fn new_with_metrics(metrics: Arc<MetricsAggregator>) -> Self {
        let config = CalodConfig::default();
        match Self::try_new_with_config_and_metrics(config.clone(), metrics.clone()).await {
            Ok(store) => store,
            Err(err) => {
                warn!(
                    "Persistence recovery failed during startup, falling back to fresh store: {}",
                    err
                );
                Self::build_from_config(config, false, metrics)
                    .await
                    .expect("fresh store initialization should succeed")
            }
        }
    }

    pub async fn try_new_with_config(config: CalodConfig) -> Result<Self, PersistenceError> {
        Self::try_new_with_config_and_metrics(config, Arc::new(MetricsAggregator::new())).await
    }

    pub async fn try_new_with_config_and_metrics(
        config: CalodConfig,
        metrics: Arc<MetricsAggregator>,
    ) -> Result<Self, PersistenceError> {
        Self::build_from_config(config, true, metrics).await
    }

    async fn build_from_config(
        config: CalodConfig,
        recover_persistence: bool,
        metrics: Arc<MetricsAggregator>,
    ) -> Result<Self, PersistenceError> {
        let persistence = config.persistence.clone();
        // Logical databases (Redis defaults to 16). `dbs[0]` is also kept as
        // `store` for persistence/INFO helpers that target the default DB.
        const NUM_DATABASES: usize = 16;
        let store = Arc::new(ShardedStore::new(config.clone()));
        let mut dbs = Vec::with_capacity(NUM_DATABASES);
        dbs.push(store.clone());
        for _ in 1..NUM_DATABASES {
            dbs.push(Arc::new(ShardedStore::new(config.clone())));
        }
        let mut command_response_cache = AHashMap::new();

        command_response_cache.insert("COMMAND", Self::precompute_command_resp());
        command_response_cache.insert("COMMAND_LIST", Self::precompute_command_list_resp());

        if recover_persistence {
            Self::recover_persistence(&store, &persistence).await?;
        }

        let aof = Self::create_aof_logger(&persistence).await;

        // A Redis-style 40-hex-char run id (two v4 UUIDs, truncated).
        let run_id = {
            let a = Uuid::new_v4().simple().to_string();
            let b = Uuid::new_v4().simple().to_string();
            format!("{}{}", a, &b[..8])
        };

        Ok(ServedCalodStore {
            store,
            dbs,
            command_response_cache,
            aof,
            metrics,
            config_overlay: Arc::new(ConfigOverlay::new()),
            run_id,
            start_time: std::time::Instant::now(),
            pubsub: Arc::new(PubSubRegistry::new()),
            watch_registry: Arc::new(WatchRegistry::new()),
            notify_flags: Arc::new(AtomicU32::new(0)),
            scripts: Arc::new(DashMap::new()),
            last_save: Arc::new(AtomicU64::new(Self::now_unix_secs())),
        })
    }

    async fn create_aof_logger(persistence: &PersistenceConfig) -> AofLogger {
        match AofLogger::new(
            persistence.enable_aof,
            persistence.aof_dir.as_path(),
            persistence.aof_fsync_strategy.clone(),
        )
        .await
        {
            Ok(logger) => logger,
            Err(err) => {
                warn!("AOF logger init failed, disabling AOF: {}", err);
                AofLogger::disabled(persistence.aof_dir.as_path())
            }
        }
    }

    async fn recover_persistence(
        store: &Arc<ShardedStore>,
        persistence: &PersistenceConfig,
    ) -> Result<(), PersistenceError> {
        if persistence.enable_snapshot {
            let first_shard = persistence.snapshot_dir.join("shard_0.bin");
            if first_shard.exists() {
                let snapshot_path = persistence.snapshot_dir.to_str().ok_or_else(|| {
                    PersistenceError::LoadFailed(
                        "Snapshot directory path is not valid UTF-8".to_string(),
                    )
                })?;
                store.load(snapshot_path).await?;
            }
        }

        if persistence.enable_aof {
            let aof_path = persistence.aof_dir.join("appendonly.aof");
            if aof_path.exists() {
                Self::replay_aof(store, aof_path.as_path()).await?;
            }
        }

        Ok(())
    }

    async fn replay_aof(store: &Arc<ShardedStore>, path: &Path) -> Result<(), PersistenceError> {
        let raw = fs::read(path).await.map_err(PersistenceError::Io)?;
        let mut position = 0usize;

        while position < raw.len() {
            let (value, consumed) = match RESPParser::parse_value_strict(&raw[position..]) {
                Ok(parsed) => parsed,
                Err(RespError::Incomplete) => {
                    warn!(
                        "Detected truncated AOF tail at byte offset {}, truncating to last valid frame",
                        position
                    );
                    Self::truncate_aof_tail(path, position as u64).await?;
                    break;
                }
                Err(err) => {
                    return Err(PersistenceError::Corruption(format!(
                        "Invalid AOF frame at byte offset {}: {:?}",
                        position, err
                    )))
                }
            };

            let command_values = match &value {
                Value::Array(array_values) => array_values.as_slice(),
                _ => std::slice::from_ref(&value),
            };
            let cmd = Command::try_from(command_values).map_err(|err| {
                PersistenceError::LoadFailed(format!(
                    "Unable to convert AOF frame to command at byte offset {}: {}",
                    position, err
                ))
            })?;

            Self::apply_replay_command(store, cmd)
                .await
                .map_err(|err| {
                    PersistenceError::LoadFailed(format!(
                        "AOF replay command failed at byte offset {}: {}",
                        position, err
                    ))
                })?;

            position += consumed;
        }

        Ok(())
    }

    async fn truncate_aof_tail(path: &Path, len: u64) -> Result<(), PersistenceError> {
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .await
            .map_err(PersistenceError::Io)?;
        file.set_len(len).await.map_err(PersistenceError::Io)
    }

    async fn apply_replay_command(
        store: &Arc<ShardedStore>,
        cmd: Command,
    ) -> Result<(), CacheError> {
        match cmd {
            Command::Set {
                key,
                value,
                expire_option,
                set_option,
            } => store
                .set_cmd(key, value, expire_option, set_option)
                .await
                .map(|_| ()),
            Command::SetNx { key, value } => store.setnx_cmd(key, value).await.map(|_| ()),
            Command::SetEx {
                key,
                seconds,
                value,
            } => store
                .set_cmd(
                    key,
                    value,
                    Some(super::command::SetExpireOption::EX(seconds)),
                    None,
                )
                .await
                .map(|_| ()),
            Command::PSetEx {
                key,
                milliseconds,
                value,
            } => store
                .set_cmd(
                    key,
                    value,
                    Some(super::command::SetExpireOption::PX(milliseconds)),
                    None,
                )
                .await
                .map(|_| ()),
            Command::Del { keys } => store.del_cmd(&keys).await.map(|_| ()),
            Command::Unlink { keys } => store.unlink_cmd(&keys).await.map(|_| ()),
            Command::Rename { key, new_key } => {
                store.rename_cmd(&key, &new_key, false).await.map(|_| ())
            }
            Command::RenameNx { key, new_key } => {
                store.rename_cmd(&key, &new_key, true).await.map(|_| ())
            }
            Command::Copy {
                source,
                destination,
                destination_db,
                replace,
            } => store
                .copy_cmd(&source, &destination, destination_db, replace)
                .await
                .map(|_| ()),
            Command::Move { key, db } => store.move_cmd(&key, db).await.map(|_| ()),
            Command::Expire { key, seconds } => store.expire_cmd(&key, seconds).await.map(|_| ()),
            Command::PExpire { key, milliseconds } => {
                store.pexpire_cmd(&key, milliseconds).await.map(|_| ())
            }
            Command::ExpireAt { key, timestamp } => {
                store.expireat_cmd(&key, timestamp).await.map(|_| ())
            }
            Command::PExpireAt {
                key,
                milliseconds_timestamp,
            } => store
                .pexpireat_cmd(&key, milliseconds_timestamp)
                .await
                .map(|_| ()),
            Command::Persist { key } => store.persist_cmd(&key).await.map(|_| ()),
            Command::FlushDb {} => store.flushdb_cmd().await.map(|_| ()),
            Command::FlushAll {} => store.flushall_cmd().await.map(|_| ()),
            Command::Append { key, value } => store.append_cmd(&key, value).await.map(|_| ()),
            Command::SetRange { key, offset, value } => {
                store.setrange_cmd(&key, offset, value).await.map(|_| ())
            }
            Command::GetSet { key, value } => store.getset_cmd(&key, value).await.map(|_| ()),
            Command::GetDel { key } => store.getdel_cmd(&key).await.map(|_| ()),
            Command::MSet { key_values } => store.mset_cmd(key_values).await.map(|_| ()),
            Command::MSetNx { key_values } => store.msetnx_cmd(key_values).await.map(|_| ()),
            Command::SAdd { key, members } => store.sadd_cmd(&key, members).await.map(|_| ()),
            Command::SRem { key, members } => store.srem_cmd(&key, members).await.map(|_| ()),
            Command::SPop { key, count } => store.spop_cmd(&key, count).await.map(|_| ()),
            Command::Incr { key } => store.incr_cmd(&key).await.map(|_| ()),
            Command::Decr { key } => store.decr_cmd(&key).await.map(|_| ()),
            Command::IncrBy { key, increment } => {
                store.incrby_cmd(&key, increment).await.map(|_| ())
            }
            Command::DecrBy { key, decrement } => {
                store.decrby_cmd(&key, decrement).await.map(|_| ())
            }
            Command::IncrByFloat { key, increment } => {
                store.incrbyfloat_cmd(&key, increment).await.map(|_| ())
            }
            Command::HSet { key, field_values } => {
                store.hset_cmd(&key, field_values).await.map(|_| ())
            }
            Command::HDel { key, fields } => store.hdel_cmd(&key, fields).await.map(|_| ()),
            Command::HIncrBy {
                key,
                field,
                increment,
            } => store.hincrby_cmd(&key, field, increment).await.map(|_| ()),
            Command::HIncrByFloat {
                key,
                key_field_increment,
            } => store
                .hincrbyfloat_cmd(&key, key_field_increment)
                .await
                .map(|_| ()),
            Command::HMSet { key, field_values } => {
                store.hmset_cmd(&key, field_values).await.map(|_| ())
            }
            Command::HSetNx { key, field, value } => {
                store.hsetnx_cmd(&key, field, value).await.map(|_| ())
            }
            Command::ListLPush { key, values } => store.lpush_cmd(key, values).await.map(|_| ()),
            Command::ListRPush { key, values } => store.rpush_cmd(key, values).await.map(|_| ()),
            Command::LPop { key } => store.lpop_cmd(&key).await.map(|_| ()),
            Command::RPop { key } => store.rpop_cmd(&key).await.map(|_| ()),
            Command::LInsert {
                key,
                before_after,
                pivot,
                value,
            } => store
                .linsert_cmd(&key, before_after, pivot, value)
                .await
                .map(|_| ()),
            Command::LSet { key, index, value } => {
                store.lset_cmd(&key, index, value).await.map(|_| ())
            }
            Command::LTrim { key, start, end } => {
                store.ltrim_cmd(&key, start, end).await.map(|_| ())
            }
            Command::LRem { key, count, value } => {
                store.lrem_cmd(&key, count, value).await.map(|_| ())
            }
            Command::RPopLPush {
                source,
                destination,
            } => store.rpoplpush_cmd(&source, &destination).await.map(|_| ()),
            Command::JsonSet { key, path, value } => {
                store.json_set_cmd(key, path, value).await.map(|_| ())
            }
            Command::JsonDel { key, path } => store.json_del_cmd(key, path).await.map(|_| ()),
            Command::JsonNumIncrBy {
                key,
                path,
                increment,
            } => store
                .json_numincrby_cmd(key, path, increment)
                .await
                .map(|_| ()),
            Command::JsonStrAppend { key, path, value } => {
                store.json_strappend_cmd(key, path, value).await.map(|_| ())
            }
            Command::JsonArrAppend { key, path, values } => store
                .json_arrappend_cmd(key, path, values)
                .await
                .map(|_| ()),
            Command::JsonObjSet {
                key,
                path,
                key_to_set,
                value,
            } => store
                .json_objset_cmd(key, path, key_to_set, value)
                .await
                .map(|_| ()),
            Command::JsonArrInsert {
                key,
                path,
                index,
                values,
            } => store
                .json_arrinsert_cmd(key, path, index, values)
                .await
                .map(|_| ()),
            Command::JsonArrPop { key, path, index } => {
                store.json_arrpop_cmd(key, path, index).await.map(|_| ())
            }
            Command::JsonArrTrim {
                key,
                path,
                start,
                stop,
            } => store
                .json_arrtrim_cmd(key, path, start, stop)
                .await
                .map(|_| ()),
            Command::GraphCreateNode {
                key,
                node_id,
                properties,
            } => store
                .graph_create_node_cmd(key, node_id, properties)
                .await
                .map(|_| ()),
            Command::GraphDeleteNode { key, node_id } => {
                store.graph_delete_node_cmd(key, node_id).await.map(|_| ())
            }
            Command::GraphCreateEdge {
                key,
                edge_id,
                source_node_id,
                target_node_id,
                relation_type,
                properties,
            } => store
                .graph_create_edge_cmd(
                    key,
                    edge_id,
                    source_node_id,
                    target_node_id,
                    relation_type,
                    properties,
                )
                .await
                .map(|_| ()),
            Command::GraphDeleteEdge { key, edge_id } => {
                store.graph_delete_edge_cmd(key, edge_id).await.map(|_| ())
            }
            Command::GraphSetNodeProperty {
                key,
                node_id,
                property_key,
                property_value,
            } => store
                .graph_set_node_property_cmd(key, node_id, property_key, property_value)
                .await
                .map(|_| ()),
            Command::GraphDeleteNodeProperty {
                key,
                node_id,
                property_key,
            } => store
                .graph_delete_node_property_cmd(key, node_id, property_key)
                .await
                .map(|_| ()),
            Command::GraphSetEdgeProperty {
                key,
                edge_id,
                property_key,
                property_value,
            } => store
                .graph_set_edge_property_cmd(key, edge_id, property_key, property_value)
                .await
                .map(|_| ()),
            Command::GraphDeleteEdgeProperty {
                key,
                edge_id,
                property_key,
            } => store
                .graph_delete_edge_property_cmd(key, edge_id, property_key)
                .await
                .map(|_| ()),
            other => Err(CacheError::InternalError(format!(
                "Command {:?} is not supported during AOF replay",
                other
            ))),
        }
    }

    async fn append_aof_if_mutating(&self, cmd: &Command) {
        if !self.aof.is_enabled() {
            return;
        }
        let should_log = Self::should_log_to_aof(cmd);

        if should_log {
            if let Some(frame) = command_to_resp_frame(cmd) {
                let _ = self.aof.append(&frame).await;
            }
        }
    }

    fn should_log_to_aof(cmd: &Command) -> bool {
        matches!(
            cmd,
            Command::Set { .. }
                | Command::SetNx { .. }
                | Command::SetEx { .. }
                | Command::PSetEx { .. }
                | Command::Del { .. }
                | Command::Unlink { .. }
                | Command::Rename { .. }
                | Command::RenameNx { .. }
                | Command::Copy { .. }
                | Command::Move { .. }
                | Command::Expire { .. }
                | Command::PExpire { .. }
                | Command::ExpireAt { .. }
                | Command::PExpireAt { .. }
                | Command::Persist { .. }
                | Command::FlushDb { .. }
                | Command::FlushAll { .. }
                | Command::Append { .. }
                | Command::SetRange { .. }
                | Command::GetSet { .. }
                | Command::GetDel { .. }
                | Command::MSet { .. }
                | Command::MSetNx { .. }
                | Command::Incr { .. }
                | Command::Decr { .. }
                | Command::IncrBy { .. }
                | Command::DecrBy { .. }
                | Command::IncrByFloat { .. }
                | Command::SAdd { .. }
                | Command::SRem { .. }
                | Command::SPop { .. }
                | Command::HSet { .. }
                | Command::HDel { .. }
                | Command::HIncrBy { .. }
                | Command::HIncrByFloat { .. }
                | Command::HMSet { .. }
                | Command::HSetNx { .. }
                | Command::ListLPush { .. }
                | Command::ListRPush { .. }
                | Command::LPop { .. }
                | Command::RPop { .. }
                | Command::LInsert { .. }
                | Command::LSet { .. }
                | Command::LTrim { .. }
                | Command::LRem { .. }
                | Command::RPopLPush { .. }
                | Command::JsonSet { .. }
                | Command::JsonDel { .. }
                | Command::JsonNumIncrBy { .. }
                | Command::JsonStrAppend { .. }
                | Command::JsonArrAppend { .. }
                | Command::JsonObjSet { .. }
                | Command::JsonArrInsert { .. }
                | Command::JsonArrPop { .. }
                | Command::JsonArrTrim { .. }
                | Command::GraphCreateNode { .. }
                | Command::GraphDeleteNode { .. }
                | Command::GraphCreateEdge { .. }
                | Command::GraphDeleteEdge { .. }
                | Command::GraphSetNodeProperty { .. }
                | Command::GraphDeleteNodeProperty { .. }
                | Command::GraphSetEdgeProperty { .. }
                | Command::GraphDeleteEdgeProperty { .. }
        )
    }

    pub fn inner_store(&self) -> Arc<ShardedStore> {
        self.store.clone()
    }

    pub async fn flush_aof(&self) -> Result<(), PersistenceError> {
        self.aof.flush().await
    }

    /// Static responses to avoid allocations
    #[inline]
    fn static_response(&self, response: &'static [u8]) -> Vec<u8> {
        response.to_vec()
    }

    // fn wrap_simple_response(&self, status: &str) -> Vec<u8> {
    //     if status == "PONG" {
    //         return self.static_response(PONG_RESPONSE);
    //     } else if status == "OK" {
    //         return self.static_response(OK_RESPONSE);
    //     }

    //     let required_size = 1 + status.len() + 2;
    //     let mut buffer = BytesMut::with_capacity(required_size);
    //     buffer.put_u8(b'+');
    //     buffer.extend_from_slice(status.as_bytes());
    //     buffer.extend_from_slice(b"\r\n");
    //     buffer.freeze().to_vec()
    // }

    /// Encode a command result into RESP wire bytes for the connection's
    /// negotiated protocol version.
    ///
    /// The storage-layer [`DataType`] is mapped to the canonical [`Value`] reply
    /// tree by [`datatype_to_value`], which [`Value::encode`] then renders as
    /// RESP2 or RESP3 (e.g. nil framing, Set `*`→`~`, Hash flat-array→`%`). This
    /// is the single reply-encoding chokepoint; per-type byte assembly no longer
    /// lives here. Errors are rendered with their canonical code prefix by
    /// [`Self::format_error_reply`].
    fn wrap_response(&self, ctx: ConnCtx, data: Result<DataType, CacheError>) -> Vec<u8> {
        match data {
            Ok(data_type) => datatype_to_value(data_type).encode(ctx.proto),
            Err(e) => {
                Self::log_command_error(&e);
                Self::format_error_reply(&e).into_bytes()
            }
        }
    }

    /// Format a `CacheError` as a complete RESP error line, terminated with CRLF,
    /// using the correct Redis error-code prefix. Clients branch on these prefixes
    /// (e.g. `WRONGTYPE`, `OOM`, `READONLY`, `WRONGPASS`), so emitting the canonical
    /// codes is required for real-client compatibility.
    /// Commands a Lua script may not invoke via `redis.call` (they would recurse,
    /// block, or have no meaning inside a script).
    fn is_disallowed_in_script(cmd: &Command) -> bool {
        use crate::cld_srv::command::Command::*;
        matches!(
            cmd,
            Eval { .. }
                | EvalSha { .. }
                | Script { .. }
                | Multi { .. }
                | Exec { .. }
                | Discard { .. }
                | Watch { .. }
                | Unwatch { .. }
                | Subscribe { .. }
                | Unsubscribe { .. }
                | PSubscribe { .. }
                | PUnsubscribe { .. }
                | BLPop { .. }
                | BRPop { .. }
        )
    }

    /// Render a script/Lua error as a RESP error line, preserving an existing
    /// error-code prefix (e.g. `WRONGTYPE`, `NOSCRIPT`) or adding `ERR`.
    fn script_error_reply(msg: &str) -> Vec<u8> {
        let first = msg.split(' ').next().unwrap_or("");
        let has_code = first.len() >= 2 && first.chars().all(|c| c.is_ascii_uppercase());
        if has_code {
            format!("-{msg}\r\n").into_bytes()
        } else {
            format!("-ERR {msg}\r\n").into_bytes()
        }
    }

    /// `EVAL`: cache the body by SHA1, then run it. `redis.call` re-enters
    /// [`Self::execute_inner`] via a channel serviced on this async task (the Lua
    /// VM itself runs on a `spawn_blocking` thread). See [`crate::cld_srv::scripting`].
    async fn handle_eval(
        &self,
        source: String,
        keys: Vec<Vec<u8>>,
        args: Vec<Vec<u8>>,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
        current_connection_arc: Arc<Mutex<Connection>>,
        ctx: ConnCtx,
    ) -> Vec<u8> {
        let sha = crate::cld_srv::scripting::sha1hex(source.as_bytes());
        self.scripts.entry(sha).or_insert_with(|| source.clone());

        let (req_tx, mut req_rx) =
            tokio::sync::mpsc::channel::<(Command, tokio::sync::oneshot::Sender<Vec<u8>>)>(8);

        let src = source;
        let blocking = tokio::task::spawn_blocking(move || {
            let call = |argv: Vec<Vec<u8>>| -> Result<crate::parser::value::Value, String> {
                if argv.is_empty() {
                    return Err("ERR wrong number of arguments".to_string());
                }
                let values: Vec<crate::parser::value::Value> = argv
                    .into_iter()
                    .map(crate::parser::value::Value::BulkString)
                    .collect();
                let cmd = Command::try_from(values.as_slice()).map_err(|e| format!("ERR {e}"))?;
                if Self::is_disallowed_in_script(&cmd) {
                    return Err(
                        "ERR This Redis command is not allowed from script".to_string()
                    );
                }
                let (otx, orx) = tokio::sync::oneshot::channel();
                req_tx
                    .blocking_send((cmd, otx))
                    .map_err(|_| "ERR script dispatch channel closed".to_string())?;
                let bytes = orx
                    .blocking_recv()
                    .map_err(|_| "ERR script reply channel closed".to_string())?;
                let (val, _) = crate::parser::parser::RESPParser::parse_value(&bytes)
                    .map_err(|_| "ERR failed to parse internal reply".to_string())?;
                match val {
                    crate::parser::value::Value::Error(e) => Err(e),
                    other => Ok(other),
                }
            };
            crate::cld_srv::scripting::run_script(&src, keys, args, &call)
        });
        tokio::pin!(blocking);

        // `redis.call` runs at RESP2 so the parser yields plain wire types.
        let inner_ctx = ConnCtx {
            proto: crate::cld_srv::protocol::ProtocolVersion::Resp2,
            ..ctx
        };
        let outcome = loop {
            tokio::select! {
                maybe = req_rx.recv() => {
                    if let Some((cmd, reply)) = maybe {
                        // Box::pin breaks the execute_inner -> handle_eval ->
                        // execute_inner async recursion cycle (otherwise the
                        // future would be infinitely sized).
                        let bytes = Box::pin(self.execute_inner(
                            cmd,
                            active_connections.clone(),
                            current_connection_arc.clone(),
                            inner_ctx,
                        )).await;
                        let _ = reply.send(bytes);
                    }
                }
                res = &mut blocking => break res,
            }
        };

        match outcome {
            Ok(Ok(value)) => value.encode(ctx.proto),
            Ok(Err(msg)) => Self::script_error_reply(&msg),
            Err(_) => b"-ERR Lua script execution failed\r\n".to_vec(),
        }
    }

    /// `EVALSHA`: look up the cached body by digest (or `NOSCRIPT`), then `EVAL`.
    async fn handle_evalsha(
        &self,
        sha: String,
        keys: Vec<Vec<u8>>,
        args: Vec<Vec<u8>>,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
        current_connection_arc: Arc<Mutex<Connection>>,
        ctx: ConnCtx,
    ) -> Vec<u8> {
        let source = match self.scripts.get(&sha) {
            Some(s) => s.clone(),
            None => return b"-NOSCRIPT No matching script. Please use EVAL.\r\n".to_vec(),
        };
        self.handle_eval(
            source,
            keys,
            args,
            active_connections,
            current_connection_arc,
            ctx,
        )
        .await
    }

    /// `SCRIPT LOAD|EXISTS|FLUSH`.
    fn handle_script(&self, sub: ScriptSubCommand, ctx: ConnCtx) -> Vec<u8> {
        match sub {
            ScriptSubCommand::Load { source } => {
                let sha = crate::cld_srv::scripting::sha1hex(source.as_bytes());
                self.scripts.insert(sha.clone(), source);
                self.wrap_response(ctx, Ok(DataType::string_from(sha)))
            }
            ScriptSubCommand::Exists { shas } => {
                let arr = shas
                    .into_iter()
                    .map(|s| DataType::Integer(i64::from(self.scripts.contains_key(&s))))
                    .collect();
                self.wrap_response(ctx, Ok(DataType::Array(arr)))
            }
            ScriptSubCommand::Flush => {
                self.scripts.clear();
                self.static_response(OK_RESPONSE)
            }
            ScriptSubCommand::NoOp => self.static_response(OK_RESPONSE),
        }
    }

    /// `RESET` — return the connection to its default post-handshake state:
    /// discard any transaction, drop pub/sub subscriptions, unwatch keys, select
    /// DB 0, and revert to RESP2. Always replies `+RESET`.
    async fn handle_reset(&self, conn_arc: Arc<Mutex<Connection>>) -> Vec<u8> {
        // Clone identity/handles out from under the lock, then mutate.
        let (client_id, txn_handle, watched) = {
            let conn = conn_arc.lock().await;
            conn.set_db_index(0);
            conn.set_protocol(crate::cld_srv::protocol::ProtocolVersion::Resp2);
            conn.set_subscription_total(0);
            let txn = conn.txn_handle();
            let watched = {
                let guard = txn.lock().await;
                guard.watched.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()
            };
            (conn.get_client_id(), txn, watched)
        };
        for key in &watched {
            self.unwatch_key(key);
        }
        {
            let mut t = txn_handle.lock().await;
            *t = TxnState::default();
        }
        self.pubsub_cleanup(client_id);
        b"+RESET\r\n".to_vec()
    }

    /// Build the reply for an `ACL` subcommand (single built-in `default` user).
    fn acl_reply(args: &[String]) -> DataType {
        let sub = args.first().map(|s| s.to_uppercase()).unwrap_or_default();
        match sub.as_str() {
            "WHOAMI" => DataType::string_from("default"),
            "LIST" => DataType::Array(vec![DataType::string_from(
                "user default on nopass sanitize-payload ~* &* +@all",
            )]),
            "CAT" => DataType::Array(
                [
                    "keyspace",
                    "read",
                    "write",
                    "set",
                    "sortedset",
                    "list",
                    "hash",
                    "string",
                    "bitmap",
                    "hyperloglog",
                    "geo",
                    "stream",
                    "pubsub",
                    "admin",
                    "fast",
                    "slow",
                    "blocking",
                    "dangerous",
                    "connection",
                    "transaction",
                    "scripting",
                ]
                .iter()
                .map(|c| DataType::string_from(*c))
                .collect(),
            ),
            "GETUSER" => DataType::Array(vec![
                DataType::string_from("flags"),
                DataType::Array(vec![
                    DataType::string_from("on"),
                    DataType::string_from("nopass"),
                ]),
                DataType::string_from("passwords"),
                DataType::Array(vec![]),
                DataType::string_from("commands"),
                DataType::string_from("+@all"),
                DataType::string_from("keys"),
                DataType::string_from("~*"),
                DataType::string_from("channels"),
                DataType::string_from("&*"),
            ]),
            "DELUSER" => DataType::Integer(0),
            // SETUSER/SAVE/LOAD and anything else: acknowledge.
            _ => DataType::Status("OK".to_string()),
        }
    }

    /// Build the reply for a `FUNCTION` subcommand (no functions registered).
    fn function_reply(args: &[String]) -> DataType {
        let sub = args.first().map(|s| s.to_uppercase()).unwrap_or_default();
        match sub.as_str() {
            "LIST" => DataType::Array(vec![]),
            "DUMP" => DataType::Nil,
            "STATS" => DataType::Array(vec![
                DataType::string_from("running_script"),
                DataType::Nil,
                DataType::string_from("engines"),
                DataType::Array(vec![]),
            ]),
            // FLUSH/RESTORE/DELETE and anything else: acknowledge.
            _ => DataType::Status("OK".to_string()),
        }
    }

    /// Current unix time in seconds (for `LASTSAVE`/`INFO`).
    fn now_unix_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// The configured snapshot directory as a string path.
    fn snapshot_dir(&self) -> String {
        self.store
            .config()
            .persistence
            .snapshot_dir
            .to_string_lossy()
            .into_owned()
    }

    /// Synchronously snapshot the dataset (DB 0) and record the save time.
    /// The bincode snapshot serializes every `DataType` variant, so all data
    /// types (strings, hashes, sorted sets, streams, HLL, …) round-trip.
    async fn snapshot_save(&self) -> Result<(), PersistenceError> {
        let dir = self.snapshot_dir();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(PersistenceError::Io)?;
        self.store.save(&dir).await?;
        self.last_save
            .store(Self::now_unix_secs(), Ordering::Relaxed);
        Ok(())
    }

    fn format_error_reply(e: &CacheError) -> String {
        /// Whether a message already begins with a Redis error-code token (a
        /// leading word of two or more uppercase ASCII letters), e.g. `ERR`,
        /// `NOGROUP`, `BUSYGROUP`, `WRONGTYPE`.
        fn has_error_code_prefix(msg: &str) -> bool {
            let first = msg.split(' ').next().unwrap_or("");
            first.len() >= 2 && first.chars().all(|c| c.is_ascii_uppercase())
        }
        match e {
            // The single most important typed error: clients special-case WRONGTYPE.
            CacheError::DataTypeMismatch(..) => {
                "-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_string()
            }
            // The message may already carry a Redis error-code prefix (an
            // explicit "ERR ...", or a typed code such as "NOGROUP"/"BUSYGROUP"/
            // "WRONGTYPE"). Detect a leading ALL-CAPS code token and pass it
            // through verbatim; otherwise prepend the default "ERR " so callers
            // can supply just the human-readable text.
            CacheError::InvalidCommandArguments(msg) => {
                if has_error_code_prefix(msg) {
                    format!("-{}\r\n", msg)
                } else {
                    format!("-ERR {}\r\n", msg)
                }
            }
            CacheError::KeyNotFound(key) => format!("-ERR Key '{}' not found\r\n", key),
            CacheError::NotAnInteger => {
                "-ERR value is not an integer or out of range\r\n".to_string()
            }
            CacheError::IndexOutOfRange => "-ERR index out of range\r\n".to_string(),
            CacheError::InvalidScoreFormat => "-ERR value is not a valid float\r\n".to_string(),
            CacheError::MemoryLimitExceeded => {
                "-OOM command not allowed when used memory > 'maxmemory'.\r\n".to_string()
            }
            CacheError::ReadOnlyMode => {
                "-READONLY You can't write against a read only replica.\r\n".to_string()
            }
            CacheError::AuthenticationFailed => {
                "-WRONGPASS invalid username-password pair or user is disabled.\r\n".to_string()
            }
            CacheError::PermissionDenied => {
                "-NOPERM this user has no permissions to run the command\r\n".to_string()
            }
            // Internal/replication/cluster/config and anything else: generic ERR.
            other => format!("-ERR {}\r\n", other),
        }
    }

    fn log_command_error(error: &CacheError) {
        match Self::command_error_log_level(error) {
            CommandErrorLogLevel::Debug => debug!("Command returned client error: {:?}", error),
            CommandErrorLogLevel::Warn => warn!("Command rejected: {:?}", error),
            CommandErrorLogLevel::Error => error!("Command failed: {:?}", error),
        }
    }

    fn command_error_log_level(error: &CacheError) -> CommandErrorLogLevel {
        match error {
            CacheError::InternalError(_)
            | CacheError::ReplicationError(_)
            | CacheError::ClusterError(_)
            | CacheError::ConfigurationError(_) => CommandErrorLogLevel::Error,
            CacheError::MemoryLimitExceeded
            | CacheError::ReadOnlyMode
            | CacheError::AuthenticationFailed
            | CacheError::PermissionDenied => CommandErrorLogLevel::Warn,
            _ => CommandErrorLogLevel::Debug,
        }
    }

    /// Top-level command entry point. Handles metrics, RESP2 subscriber-mode
    /// gating, and transaction control (MULTI/EXEC/WATCH queueing), then
    /// delegates the actual work to [`Self::execute_inner`].
    #[instrument(
        level = "trace",
        skip(self, cmd, active_connections, current_connection_arc),
        ret
    )]
    pub async fn execute(
        &self,
        cmd: Command,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
        current_connection_arc: Arc<Mutex<Connection>>,
        ctx: ConnCtx,
    ) -> Vec<u8> {
        self.metrics.command_processed_inc();

        // RESP2 subscriber mode: while a RESP2 connection holds any
        // subscription, only the (P|S)(UN)SUBSCRIBE / PING / QUIT commands are
        // permitted, plus HELLO (so a subscribed client can still upgrade to
        // RESP3). RESP3 has no such restriction (push frames are out-of-band).
        if ctx.subscribed && !ctx.proto.is_resp3() {
            let allowed = matches!(
                &cmd,
                Command::Subscribe { .. }
                    | Command::Unsubscribe { .. }
                    | Command::PSubscribe { .. }
                    | Command::PUnsubscribe { .. }
                    | Command::Ping {}
                    | Command::Quit {}
                    | Command::Hello { .. }
            );
            if !allowed {
                return b"-ERR Can't execute command: only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / HELLO are allowed in this context\r\n".to_vec();
            }
        }

        // Transaction control commands act on the connection's MULTI state and
        // never enter the normal queue/dispatch path.
        match &cmd {
            Command::Multi {} => return self.handle_multi(current_connection_arc).await,
            Command::Exec {} => {
                return self
                    .handle_exec(active_connections, current_connection_arc, ctx)
                    .await
            }
            Command::Discard {} => return self.handle_discard(current_connection_arc).await,
            Command::Watch { keys } => {
                return self
                    .handle_watch(keys.clone(), current_connection_arc)
                    .await
            }
            Command::Unwatch {} => return self.handle_unwatch(current_connection_arc).await,
            _ => {}
        }

        // Queue phase: inside MULTI, accumulate commands instead of running them.
        {
            let txn_arc = current_connection_arc.lock().await.txn_handle();
            let mut txn = txn_arc.lock().await;
            if txn.queued.is_some() {
                if matches!(cmd, Command::Unknown(_)) {
                    // A command that fails to parse poisons the transaction so
                    // EXEC aborts with EXECABORT; reply with the parse error now.
                    txn.dirty = true;
                    drop(txn);
                    return self
                        .execute_inner(cmd, active_connections, current_connection_arc, ctx)
                        .await;
                }
                txn.queued.as_mut().unwrap().push(cmd);
                return b"+QUEUED\r\n".to_vec();
            }
        }

        self.execute_inner(cmd, active_connections, current_connection_arc, ctx)
            .await
    }

    /// Execute a single command with no transaction/subscriber-mode handling.
    /// Called for normal commands and once per queued command by EXEC.
    async fn execute_inner(
        &self,
        cmd: Command,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
        current_connection_arc: Arc<Mutex<Connection>>,
        ctx: ConnCtx,
    ) -> Vec<u8> {
        match &cmd {
            Command::Ping {} => return self.static_response(PONG_RESPONSE),
            Command::Quit {} => return b"+OK\r\n".to_vec(),
            Command::Hello {
                protover,
                auth,
                setname,
            } => {
                return self
                    .handle_hello(
                        *protover,
                        auth.clone(),
                        setname.clone(),
                        ctx,
                        current_connection_arc,
                    )
                    .await;
            }
            Command::Echo { message } => {
                return self.wrap_response(ctx, Ok(DataType::string_from(message.clone())))
            }
            Command::Eval {
                source, keys, args, ..
            } => {
                return self
                    .handle_eval(
                        source.clone(),
                        keys.clone(),
                        args.clone(),
                        active_connections,
                        current_connection_arc,
                        ctx,
                    )
                    .await;
            }
            Command::EvalSha {
                sha, keys, args, ..
            } => {
                return self
                    .handle_evalsha(
                        sha.clone(),
                        keys.clone(),
                        args.clone(),
                        active_connections,
                        current_connection_arc,
                        ctx,
                    )
                    .await;
            }
            Command::Script { sub } => return self.handle_script(sub.clone(), ctx),
            Command::Reset {} => return self.handle_reset(current_connection_arc).await,
            Command::CommandList {} => {
                return self
                    .command_response_cache
                    .get("COMMAND_LIST")
                    .cloned()
                    .unwrap_or_else(|| {
                        self.wrap_response(
                            ctx,
                            Ok(DataType::List(
                                Self::get_command_list_resp()
                                    .into_iter()
                                    .map(String::into_bytes)
                                    .collect(),
                            )),
                        )
                    })
            }
            Command::Command {} => {
                return self
                    .command_response_cache
                    .get("COMMAND")
                    .cloned()
                    .unwrap_or_else(|| {
                        self.wrap_response(
                            ctx,
                            Ok(DataType::List(
                                Self::get_command_resp()
                                    .into_iter()
                                    .map(String::into_bytes)
                                    .collect(),
                            )),
                        )
                    })
            }
            Command::Client { subcommand } => {
                return self
                    .handle_client_command(
                        subcommand.clone(),
                        active_connections,
                        current_connection_arc,
                        ctx,
                    )
                    .await;
            }
            Command::Info { section } => return self.info(section.as_deref()).await,
            Command::Auth { .. } => return self.static_response(OK_RESPONSE),
            Command::Config { sub } => return self.handle_config(sub.clone(), ctx),
            Command::CommandCount {} => {
                let count = Self::get_command_list_resp().len() as i64;
                return self.wrap_response(ctx, Ok(DataType::Integer(count)));
            }
            Command::CommandDocs { .. } => {
                // An empty map is a valid COMMAND DOCS reply; clients (redis-py)
                // call this on connect and tolerate an empty result.
                return Value::Map(vec![]).encode(ctx.proto);
            }
            // Pub/Sub commands: handled on the push path, never via the storage
            // layer or AOF. SUBSCRIBE/UNSUBSCRIBE confirmations and PUBLISH fan-out
            // require the connection handle (push sender, subscription counter).
            Command::Subscribe { channels } => {
                return self
                    .handle_subscribe(channels.clone(), false, ctx, current_connection_arc)
                    .await;
            }
            Command::Unsubscribe { channels } => {
                return self
                    .handle_unsubscribe(channels.clone(), false, ctx, current_connection_arc)
                    .await;
            }
            Command::PSubscribe { patterns } => {
                return self
                    .handle_subscribe(patterns.clone(), true, ctx, current_connection_arc)
                    .await;
            }
            Command::PUnsubscribe { patterns } => {
                return self
                    .handle_unsubscribe(patterns.clone(), true, ctx, current_connection_arc)
                    .await;
            }
            Command::Publish { channel, message } => {
                return self.handle_publish(channel, message, ctx);
            }
            Command::PubSubChannels { pattern } => {
                return self.handle_pubsub_channels(pattern.as_deref(), ctx);
            }
            Command::PubSubNumSub { channels } => {
                return self.handle_pubsub_numsub(channels, ctx);
            }
            Command::PubSubNumPat {} => {
                return self.handle_pubsub_numpat(ctx);
            }
            _ => {}
        }

        debug!("Executing command: {:?}", cmd);

        // Route data commands to the connection's currently-SELECTed database.
        // `dbs[0]` aliases `self.store`; an out-of-range index falls back to it.
        let store = self.dbs.get(ctx.db as usize).unwrap_or(&self.store);
        let result = match cmd.clone() {
            Command::Ping {}
            | Command::Quit {}
            | Command::Hello { .. }
            | Command::Echo { .. }
            | Command::CommandList {}
            | Command::Command {}
            | Command::CommandCount {}
            | Command::CommandDocs { .. }
            | Command::Client { .. }
            | Command::Config { .. }
            | Command::Info { .. }
            | Command::Auth { .. }
            | Command::Subscribe { .. }
            | Command::Unsubscribe { .. }
            | Command::PSubscribe { .. }
            | Command::PUnsubscribe { .. }
            | Command::Publish { .. }
            | Command::PubSubChannels { .. }
            | Command::PubSubNumSub { .. }
            | Command::PubSubNumPat {}
            | Command::Multi {}
            | Command::Exec {}
            | Command::Discard {}
            | Command::Watch { .. }
            | Command::Unwatch {}
            | Command::Eval { .. }
            | Command::EvalSha { .. }
            | Command::Script { .. }
            | Command::Reset {} => {
                // Already handled above (connection/pubsub/transaction/scripting)
                unreachable!()
            }
            Command::CommandHelp { command_name } => Ok(DataType::List(
                Self::get_command_help_resp(&command_name)
                    .into_iter()
                    .map(String::into_bytes)
                    .collect(),
            )),
            Command::CommandInfo { command_name } => Ok(DataType::List(
                Self::get_command_info_resp(&command_name)
                    .into_iter()
                    .map(String::into_bytes)
                    .collect(),
            )),

            Command::Type { key } => store.type_cmd(&key).await,
            Command::Keys { pattern } => store.keys_cmd(&pattern).await,
            Command::RandomKey {} => store.randomkey_cmd().await,
            Command::Rename { key, new_key } => store.rename_cmd(&key, &new_key, false).await,
            Command::RenameNx { key, new_key } => store.rename_cmd(&key, &new_key, true).await,
            Command::Copy {
                source,
                destination,
                destination_db,
                replace,
            } => {
                match destination_db {
                    // Same DB (or unspecified): the store handles it directly.
                    None => store.copy_cmd(&source, &destination, None, replace).await,
                    Some(db) if db as usize == ctx.db as usize => {
                        store.copy_cmd(&source, &destination, None, replace).await
                    }
                    Some(db) if (db as usize) < self.dbs.len() => {
                        let dest = &self.dbs[db as usize];
                        if !replace && dest.exists(&destination).await {
                            Ok(DataType::Integer(0))
                        } else if let Some(entry) = store.get_entry(&source).await {
                            dest.put_entry(destination.clone(), entry).await;
                            Ok(DataType::Integer(1))
                        } else {
                            Ok(DataType::Integer(0))
                        }
                    }
                    Some(_) => return b"-ERR DB index is out of range\r\n".to_vec(),
                }
            }
            Command::Move { key, db } => {
                let target = db as usize;
                if target == ctx.db as usize {
                    return b"-ERR source and destination objects are the same\r\n".to_vec();
                }
                if target >= self.dbs.len() {
                    return b"-ERR DB index is out of range\r\n".to_vec();
                }
                let dest = &self.dbs[target];
                // MOVE only when the key exists in the source and not the dest.
                if dest.exists(&key).await {
                    Ok(DataType::Integer(0))
                } else if let Some(entry) = store.take_entry(&key).await {
                    dest.put_entry(key.clone(), entry).await;
                    Ok(DataType::Integer(1))
                } else {
                    Ok(DataType::Integer(0))
                }
            }
            Command::Exists { keys } => store.exists_cmd(&keys).await,
            Command::Touch { keys } => store.touch_cmd(&keys).await,
            Command::Expire { key, seconds } => store.expire_cmd(&key, seconds).await,
            Command::PExpire { key, milliseconds } => {
                store.pexpire_cmd(&key, milliseconds).await
            }
            Command::ExpireAt { key, timestamp } => store.expireat_cmd(&key, timestamp).await,
            Command::PExpireAt {
                key,
                milliseconds_timestamp,
            } => store.pexpireat_cmd(&key, milliseconds_timestamp).await,
            Command::Ttl { key } => store.ttl_cmd(&key).await,
            Command::Pttl { key } => store.pttl_cmd(&key).await,
            Command::ExpireTime { key } => store.expiretime_cmd(&key).await,
            Command::PExpireTime { key } => store.pexpiretime_cmd(&key).await,
            Command::Persist { key } => store.persist_cmd(&key).await,
            Command::ObjectEncoding { key } => store.object_encoding_cmd(&key).await,
            Command::ObjectRefCount { key } => store.object_refcount_cmd(&key).await,
            Command::ObjectIdleTime { key } => store.object_idletime_cmd(&key).await,
            Command::Del { keys } => store.del_cmd(&keys).await,
            Command::Unlink { keys } => store.unlink_cmd(&keys).await,
            Command::DbSize {} => store.dbsize_cmd().await,
            Command::FlushDb {} => store.flushdb_cmd().await,
            Command::FlushAll {} => {
                // FLUSHALL clears every logical database, not just the current one.
                for db in &self.dbs {
                    let _ = db.flushall_cmd().await;
                }
                Ok(DataType::Status("OK".to_string()))
            }
            Command::Select { db } => {
                if (db as usize) < self.dbs.len() {
                    current_connection_arc.lock().await.set_db_index(db as u8);
                    return self.static_response(OK_RESPONSE);
                } else {
                    return b"-ERR DB index is out of range\r\n".to_vec();
                }
            }
            Command::Scan {
                cursor,
                pattern,
                count,
            } => store.scan_cmd(cursor, pattern, count).await,
            Command::Wait {
                numreplicas,
                timeout_ms,
            } => store.wait_cmd(numreplicas, timeout_ms).await,

            Command::Set {
                key,
                value,
                expire_option,
                set_option,
            } => match store
                .set_cmd(key, value, expire_option, set_option)
                .await
            {
                Ok(true) => Ok(DataType::Status("OK".to_string())),
                Ok(false) => Ok(DataType::Nil),
                Err(e) => Err(e),
            },
            Command::SetNx { key, value } => store.setnx_cmd(key, value).await,
            Command::SetEx {
                key,
                seconds,
                value,
            } => store
                .set_cmd(
                    key,
                    value,
                    Some(super::command::SetExpireOption::EX(seconds)),
                    None,
                )
                .await
                .map(|_| DataType::Status("OK".to_string())),
            Command::PSetEx {
                key,
                milliseconds,
                value,
            } => store
                .set_cmd(
                    key,
                    value,
                    Some(super::command::SetExpireOption::PX(milliseconds)),
                    None,
                )
                .await
                .map(|_| DataType::Status("OK".to_string())),
            Command::Get { key } => store.get_cmd(&key).await,
            Command::GetDel { key } => store.getdel_cmd(&key).await,
            Command::GetEx { key, option } => store.getex_cmd(&key, option).await,
            Command::Append { key, value } => store.append_cmd(&key, value).await,
            Command::StrLen { key } => store.strlen_cmd(&key).await,
            Command::GetRange { key, start, end } => {
                store.getrange_cmd(&key, start, end).await
            }
            Command::SetRange { key, offset, value } => {
                store.setrange_cmd(&key, offset, value).await
            }
            Command::GetSet { key, value } => store.getset_cmd(&key, value).await,
            Command::MGet { keys } => store.mget_cmd(&keys).await,
            Command::MSet { key_values } => match store.mset_cmd(key_values).await {
                Ok(_) => Ok(DataType::Status("OK".to_string())),
                Err(e) => Err(e),
            },
            Command::MSetNx { key_values } => {
                if key_values.is_empty() {
                    return b"-ERR wrong number of arguments for 'msetnx' command\r\n".to_vec();
                }
                store.msetnx_cmd(key_values).await
            }
            Command::SAdd { key, members } => store.sadd_cmd(&key, members).await,
            Command::SRem { key, members } => store.srem_cmd(&key, members).await,
            Command::SMembers { key } => store.smembers_cmd(&key).await,
            Command::SIsMember { key, member } => store.sismember_cmd(&key, &member).await,
            Command::SCard { key } => store.scard_cmd(&key).await,
            Command::SPop { key, count } => store.spop_cmd(&key, count).await,
            Command::SRandMember { key, count } => store.srandmember_cmd(&key, count).await,
            Command::Incr { key } => store.incr_cmd(&key).await,
            Command::Decr { key } => store.decr_cmd(&key).await,
            Command::IncrBy { key, increment } => store.incrby_cmd(&key, increment).await,
            Command::DecrBy { key, decrement } => store.decrby_cmd(&key, decrement).await,
            Command::IncrByFloat { key, increment } => {
                store.incrbyfloat_cmd(&key, increment).await
            }

            Command::HSet { key, field_values } => store.hset_cmd(&key, field_values).await,
            Command::HGet { key, field } => store.hget_cmd(&key, field).await,
            Command::HDel { key, fields } => store.hdel_cmd(&key, fields).await,
            Command::HExists { key, field } => store.hexists_cmd(&key, field).await,
            Command::HGetAll { key } => store.hgetall_cmd(&key).await,
            Command::HIncrBy {
                key,
                field,
                increment,
            } => store.hincrby_cmd(&key, field, increment).await,
            Command::HIncrByFloat {
                key,
                key_field_increment,
            } => store.hincrbyfloat_cmd(&key, key_field_increment).await,
            Command::HKeys { key } => store.hkeys_cmd(&key).await,
            Command::HLen { key } => store.hlen_cmd(&key).await,
            Command::HMGet { key, fields } => store.hmget_cmd(&key, fields).await,
            Command::HMSet { key, field_values } => {
                match store.hmset_cmd(&key, field_values).await {
                    Ok(_) => Ok(DataType::Status("OK".to_string())),
                    Err(e) => Err(e),
                }
            }
            Command::HSetNx { key, field, value } => {
                store.hsetnx_cmd(&key, field, value).await
            }
            Command::HVals { key } => store.hvals_cmd(&key).await,

            Command::ListLPush { key, values } => store.lpush_cmd(key, values).await,
            Command::ListRPush { key, values } => store.rpush_cmd(key, values).await,
            Command::LPop { key } => store.lpop_cmd(&key).await,
            Command::RPop { key } => store.rpop_cmd(&key).await,
            Command::LLen { key } => store.llen_cmd(&key).await,
            Command::LRange { key, start, end } => store.lrange_cmd(&key, start, end).await,
            Command::LIndex { key, index } => store.lindex_cmd(&key, index).await,
            Command::LInsert {
                key,
                before_after,
                pivot,
                value,
            } => {
                store
                    .linsert_cmd(&key, before_after, pivot, value)
                    .await
            }
            Command::LSet { key, index, value } => store.lset_cmd(&key, index, value).await,
            Command::LTrim { key, start, end } => store.ltrim_cmd(&key, start, end).await,
            Command::LRem { key, count, value } => store.lrem_cmd(&key, count, value).await,
            Command::RPopLPush {
                source,
                destination,
            } => store.rpoplpush_cmd(&source, &destination).await,
            Command::BLPop { keys, timeout } => store.blpop_cmd(keys, timeout).await,
            Command::BRPop { keys, timeout } => store.brpop_cmd(keys, timeout).await,

            Command::JsonSet { key, path, value } => {
                store.json_set_cmd(key, path, value).await
            }
            Command::JsonGet { key, path } => store.json_get_cmd(key, path).await,
            Command::JsonDel { key, path } => store.json_del_cmd(key, path).await,
            Command::JsonType { key, path } => store.json_type_cmd(key, path).await,
            Command::JsonNumIncrBy {
                key,
                path,
                increment,
            } => store.json_numincrby_cmd(key, path, increment).await,
            Command::JsonStrAppend { key, path, value } => {
                store.json_strappend_cmd(key, path, value).await
            }
            Command::JsonArrAppend { key, path, values } => {
                store.json_arrappend_cmd(key, path, values).await
            }
            Command::JsonObjSet {
                key,
                path,
                key_to_set,
                value,
            } => {
                store
                    .json_objset_cmd(key, path, key_to_set, value)
                    .await
            }
            Command::JsonObjKeys { key, path } => store.json_objkeys_cmd(key, path).await,
            Command::JsonObjLen { key, path } => store.json_objlen_cmd(key, path).await,
            Command::JsonArrIndex {
                key,
                path,
                value,
                range,
            } => store.json_arrindex_cmd(key, path, value, range).await,
            Command::JsonArrInsert {
                key,
                path,
                index,
                values,
            } => {
                store
                    .json_arrinsert_cmd(key, path, index, values)
                    .await
            }
            Command::JsonArrLen { key, path } => store.json_arrlen_cmd(key, path).await,
            Command::JsonArrPop { key, path, index } => {
                store.json_arrpop_cmd(key, path, index).await
            }
            Command::JsonArrTrim {
                key,
                path,
                start,
                stop,
            } => store.json_arrtrim_cmd(key, path, start, stop).await,

            Command::GraphCreateNode {
                key,
                node_id,
                properties,
            } => {
                store
                    .graph_create_node_cmd(key, node_id, properties)
                    .await
            }
            Command::GraphGetNode { key, node_id } => {
                store.graph_get_node_cmd(key, node_id).await
            }
            Command::GraphDeleteNode { key, node_id } => {
                store.graph_delete_node_cmd(key, node_id).await
            }
            Command::GraphCreateEdge {
                key,
                edge_id,
                source_node_id,
                target_node_id,
                relation_type,
                properties,
            } => {
                store
                    .graph_create_edge_cmd(
                        key,
                        edge_id,
                        source_node_id,
                        target_node_id,
                        relation_type,
                        properties,
                    )
                    .await
            }
            Command::GraphGetEdge { key, edge_id } => {
                store.graph_get_edge_cmd(key, edge_id).await
            }
            Command::GraphDeleteEdge { key, edge_id } => {
                store.graph_delete_edge_cmd(key, edge_id).await
            }
            Command::GraphGetNodeProperties { key, node_id } => {
                store.graph_get_node_properties_cmd(key, node_id).await
            }
            Command::GraphSetNodeProperty {
                key,
                node_id,
                property_key,
                property_value,
            } => {
                store
                    .graph_set_node_property_cmd(key, node_id, property_key, property_value)
                    .await
            }
            Command::GraphDeleteNodeProperty {
                key,
                node_id,
                property_key,
            } => {
                store
                    .graph_delete_node_property_cmd(key, node_id, property_key)
                    .await
            }
            Command::GraphGetEdgeProperties { key, edge_id } => {
                store.graph_get_edge_properties_cmd(key, edge_id).await
            }
            Command::GraphSetEdgeProperty {
                key,
                edge_id,
                property_key,
                property_value,
            } => {
                store
                    .graph_set_edge_property_cmd(key, edge_id, property_key, property_value)
                    .await
            }
            Command::GraphDeleteEdgeProperty {
                key,
                edge_id,
                property_key,
            } => {
                store
                    .graph_delete_edge_property_cmd(key, edge_id, property_key)
                    .await
            }

            // Sorted set commands (legacy REV variants are unified via a `rev`
            // flag; for *REV*BYSCORE/BYLEX the parser already supplies min/max as
            // low/high bounds).
            Command::ZAdd {
                key,
                nx,
                xx,
                gt,
                lt,
                ch,
                incr,
                members,
            } => {
                store
                    .zadd_cmd(&key, nx, xx, gt, lt, ch, incr, members)
                    .await
            }
            Command::ZScore { key, member } => store.zscore_cmd(&key, &member).await,
            Command::ZMScore { key, members } => store.zmscore_cmd(&key, &members).await,
            Command::ZCard { key } => store.zcard_cmd(&key).await,
            Command::ZIncrBy {
                key,
                increment,
                member,
            } => store.zincrby_cmd(&key, increment, member).await,
            Command::ZRem { key, members } => store.zrem_cmd(&key, &members).await,
            Command::ZRank {
                key,
                member,
                with_score,
            } => store.zrank_cmd(&key, &member, false, with_score).await,
            Command::ZRevRank {
                key,
                member,
                with_score,
            } => store.zrank_cmd(&key, &member, true, with_score).await,
            Command::ZRange {
                key,
                start,
                stop,
                with_scores,
            } => {
                store
                    .zrange_cmd(&key, start, stop, false, with_scores)
                    .await
            }
            Command::ZRevRange {
                key,
                start,
                stop,
                with_scores,
            } => {
                store
                    .zrange_cmd(&key, start, stop, true, with_scores)
                    .await
            }
            Command::ZRangeByScore {
                key,
                min,
                max,
                with_scores,
                limit,
            } => {
                store
                    .zrangebyscore_cmd(&key, &min, &max, false, with_scores, limit)
                    .await
            }
            Command::ZRevRangeByScore {
                key,
                max,
                min,
                with_scores,
                limit,
            } => {
                store
                    .zrangebyscore_cmd(&key, &min, &max, true, with_scores, limit)
                    .await
            }
            Command::ZRangeByLex {
                key,
                min,
                max,
                limit,
            } => {
                store
                    .zrangebylex_cmd(&key, &min, &max, false, limit)
                    .await
            }
            Command::ZRevRangeByLex {
                key,
                max,
                min,
                limit,
            } => {
                store
                    .zrangebylex_cmd(&key, &min, &max, true, limit)
                    .await
            }
            Command::ZCount { key, min, max } => store.zcount_cmd(&key, &min, &max).await,
            Command::ZLexCount { key, min, max } => {
                store.zlexcount_cmd(&key, &min, &max).await
            }
            Command::ZPopMin { key, count } => store.zpopmin_cmd(&key, count).await,
            Command::ZPopMax { key, count } => store.zpopmax_cmd(&key, count).await,
            Command::ZRemRangeByRank { key, start, stop } => {
                store.zremrangebyrank_cmd(&key, start, stop).await
            }
            Command::ZRemRangeByScore { key, min, max } => {
                store.zremrangebyscore_cmd(&key, &min, &max).await
            }
            Command::ZRemRangeByLex { key, min, max } => {
                store.zremrangebylex_cmd(&key, &min, &max).await
            }
            Command::ZRandMember {
                key,
                count,
                with_scores,
            } => store.zrandmember_cmd(&key, count, with_scores).await,
            Command::ZScan {
                key,
                cursor,
                pattern,
                count,
            } => store.zscan_cmd(&key, cursor, pattern, count).await,

            // Bitmap & HyperLogLog commands
            Command::SetBit { key, offset, value } => {
                store.setbit_cmd(&key, offset, value).await
            }
            Command::GetBit { key, offset } => store.getbit_cmd(&key, offset).await,
            Command::BitCount {
                key,
                range,
                bit_unit,
            } => store.bitcount_cmd(&key, range, bit_unit).await,
            Command::BitPos {
                key,
                bit,
                start,
                end,
                bit_unit,
            } => store.bitpos_cmd(&key, bit, start, end, bit_unit).await,
            Command::BitOp { op, dest, keys } => store.bitop_cmd(&op, &dest, &keys).await,
            Command::PfAdd { key, elements } => store.pfadd_cmd(&key, &elements).await,
            Command::PfCount { keys } => store.pfcount_cmd(&keys).await,
            Command::PfMerge { dest, sources } => store.pfmerge_cmd(&dest, &sources).await,

            // Geo commands (stored as geohash scores in a sorted set). `unit_div`
            // is meters-per-unit; radius is converted to meters before the scan.
            Command::GeoAdd {
                key,
                nx,
                xx,
                ch,
                items,
            } => store.geoadd_cmd(&key, nx, xx, ch, items).await,
            Command::GeoPos { key, members } => store.geopos_cmd(&key, &members).await,
            Command::GeoDist {
                key,
                member1,
                member2,
                unit,
            } => {
                store
                    .geodist_cmd(&key, &member1, &member2, geo_unit_div(&unit))
                    .await
            }
            Command::GeoHash { key, members } => store.geohash_cmd(&key, &members).await,
            Command::GeoSearch {
                key,
                from_member,
                from_lonlat,
                radius,
                unit,
                with_coord,
                with_dist,
                with_hash,
                count,
                asc,
            } => {
                let div = geo_unit_div(&unit);
                store
                    .geosearch_cmd(
                        &key,
                        from_member.as_deref(),
                        from_lonlat,
                        radius * div,
                        div,
                        with_coord,
                        with_dist,
                        with_hash,
                        count,
                        asc,
                    )
                    .await
            }

            // Stream commands (append-only log, Phase 8 core).
            Command::XAdd {
                key,
                id,
                nomkstream,
                maxlen,
                fields,
            } => {
                store
                    .xadd_cmd(&key, &id, nomkstream, maxlen, fields)
                    .await
            }
            Command::XLen { key } => store.xlen_cmd(&key).await,
            Command::XRange {
                key,
                start,
                end,
                count,
            } => store.xrange_cmd(&key, &start, &end, count, false).await,
            Command::XRevRange {
                key,
                end,
                start,
                count,
            } => store.xrange_cmd(&key, &start, &end, count, true).await,
            Command::XDel { key, ids } => store.xdel_cmd(&key, &ids).await,
            Command::XTrim { key, maxlen } => store.xtrim_cmd(&key, maxlen).await,
            Command::XRead { count, streams } => store.xread_cmd(&streams, count).await,

            // Stream consumer groups (Phase 8b).
            Command::XGroupCreate {
                key,
                group,
                id,
                mkstream,
            } => store.xgroup_create_cmd(&key, &group, &id, mkstream).await,
            Command::XGroupSetId { key, group, id } => {
                store.xgroup_setid_cmd(&key, &group, &id).await
            }
            Command::XGroupDestroy { key, group } => {
                store.xgroup_destroy_cmd(&key, &group).await
            }
            Command::XGroupCreateConsumer {
                key,
                group,
                consumer,
            } => {
                store
                    .xgroup_createconsumer_cmd(&key, &group, &consumer)
                    .await
            }
            Command::XGroupDelConsumer {
                key,
                group,
                consumer,
            } => {
                store
                    .xgroup_delconsumer_cmd(&key, &group, &consumer)
                    .await
            }
            Command::XReadGroup {
                group,
                consumer,
                count,
                noack,
                streams,
            } => {
                store
                    .xreadgroup_cmd(&group, &consumer, count, noack, &streams)
                    .await
            }
            Command::XAck { key, group, ids } => store.xack_cmd(&key, &group, &ids).await,
            Command::XPending {
                key,
                group,
                start,
                end,
                count,
                consumer,
                idle,
            } => match (start, end, count) {
                (Some(s), Some(e), Some(c)) => {
                    store
                        .xpending_extended_cmd(&key, &group, idle, &s, &e, c, consumer.as_deref())
                        .await
                }
                _ => store.xpending_summary_cmd(&key, &group).await,
            },
            Command::XClaim {
                key,
                group,
                consumer,
                min_idle,
                ids,
                idle,
                time,
                retrycount,
                force,
                justid,
            } => {
                store
                    .xclaim_cmd(
                        &key, &group, &consumer, min_idle, &ids, idle, time, retrycount, force,
                        justid,
                    )
                    .await
            }
            Command::XAutoClaim {
                key,
                group,
                consumer,
                min_idle,
                start,
                count,
                justid,
            } => {
                store
                    .xautoclaim_cmd(&key, &group, &consumer, min_idle, &start, count, justid)
                    .await
            }
            Command::XInfoStream { key } => store.xinfo_stream_cmd(&key).await,
            Command::XInfoGroups { key } => store.xinfo_groups_cmd(&key).await,
            Command::XInfoConsumers { key, group } => {
                store.xinfo_consumers_cmd(&key, &group).await
            }
            Command::XSetId {
                key,
                id,
                entries_added,
                max_deleted_id,
            } => {
                store
                    .xsetid_cmd(&key, &id, entries_added, max_deleted_id.as_deref())
                    .await
            }

            // ---- Admin / introspection (Phase 13) ----
            Command::Debug { args } => {
                let sub = args.first().map(|s| s.to_uppercase()).unwrap_or_default();
                match sub.as_str() {
                    "SLEEP" => {
                        if let Some(secs) = args.get(1).and_then(|s| s.parse::<f64>().ok()) {
                            tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
                        }
                        Ok(DataType::Status("OK".to_string()))
                    }
                    "OBJECT" => {
                        let key = args.get(1).cloned().unwrap_or_default();
                        match store.object_encoding_cmd(&key).await {
                            Ok(DataType::String(enc)) => {
                                let encoding = String::from_utf8_lossy(&enc);
                                let len =
                                    store.get_entry(&key).await.map(|e| e.size()).unwrap_or(0);
                                Ok(DataType::Status(format!(
                                    "Value at:0x0 refcount:1 encoding:{encoding} serializedlength:{len} lru:0 lru_seconds_idle:0"
                                )))
                            }
                            Ok(DataType::Nil) => Err(CacheError::InvalidCommandArguments(
                                "ERR no such key".to_string(),
                            )),
                            other => other,
                        }
                    }
                    "RELOAD" => {
                        // Persist to disk then reload — exercises the snapshot
                        // round-trip for every data type currently held.
                        let reload = async {
                            self.snapshot_save().await?;
                            self.store.load(&self.snapshot_dir()).await
                        };
                        match reload.await {
                            Ok(()) => Ok(DataType::Status("OK".to_string())),
                            Err(e) => Err(CacheError::InvalidCommandArguments(format!(
                                "ERR {e}"
                            ))),
                        }
                    }
                    // Everything else (JMAP, SET-ACTIVE-EXPIRE, …) is a no-op ack.
                    _ => Ok(DataType::Status("OK".to_string())),
                }
            }
            Command::SlowLog { sub, arg: _ } => match sub.as_str() {
                "RESET" => Ok(DataType::Status("OK".to_string())),
                "LEN" => Ok(DataType::Integer(0)),
                _ => Ok(DataType::Array(vec![])),
            },
            Command::Memory { sub, key } => match sub.as_str() {
                "USAGE" => {
                    let k = key.unwrap_or_default();
                    match store.get_entry(&k).await {
                        Some(e) => Ok(DataType::Integer((e.size() + 64) as i64)),
                        None => Ok(DataType::Nil),
                    }
                }
                "DOCTOR" => Ok(DataType::string_from(
                    "Sam, I detected a few issues in this Redis instance memory implants:\n\n * Everything looks healthy: no problems detected.\n",
                )),
                "STATS" => Ok(DataType::Array(vec![])),
                _ => Ok(DataType::Status("OK".to_string())),
            },
            Command::Latency { sub } => match sub.as_str() {
                "RESET" => Ok(DataType::Integer(0)),
                "DOCTOR" => Ok(DataType::string_from(
                    "Dave, I have observed the system, no worrying latency spikes. Everything seems fine.",
                )),
                _ => Ok(DataType::Array(vec![])),
            },
            Command::Lolwut {} => Ok(DataType::string_from(
                "calod 7.4.0 — a Redis-compatible store\n",
            )),
            Command::Acl { args } => Ok(Self::acl_reply(&args)),
            Command::Function { args } => Ok(Self::function_reply(&args)),
            Command::WaitAof { .. } => Ok(DataType::Array(vec![
                DataType::Integer(0),
                DataType::Integer(0),
            ])),
            Command::Failover {} => Err(CacheError::InvalidCommandArguments(
                "ERR FAILOVER requires connected replicas.".to_string(),
            )),
            Command::Save {} => match self.snapshot_save().await {
                Ok(()) => Ok(DataType::Status("OK".to_string())),
                Err(e) => Err(CacheError::InvalidCommandArguments(format!(
                    "ERR {e}"
                ))),
            },
            Command::BgSave {} => {
                // Snapshot off-task so the caller isn't blocked.
                let store = self.store.clone();
                let dir = self.snapshot_dir();
                let last_save = self.last_save.clone();
                tokio::spawn(async move {
                    if store.save(&dir).await.is_ok() {
                        last_save.store(Self::now_unix_secs(), Ordering::Relaxed);
                    }
                });
                Ok(DataType::Status("Background saving started".to_string()))
            }
            Command::LastSave {} => {
                Ok(DataType::Integer(self.last_save.load(Ordering::Relaxed) as i64))
            }
            Command::BgRewriteAof {} => Ok(DataType::Status(
                "Background append only file rewriting started".to_string(),
            )),

            Command::Unknown(command_str) => {
                debug!("Unknown command received: {}", command_str);
                let msg = if command_str.starts_with("ERR ")
                    || command_str.starts_with("NOAUTH")
                    || command_str.starts_with("WRONGPASS")
                {
                    command_str
                } else {
                    format!("ERR unknown command '{}'", command_str)
                };
                return format!("-{}\r\n", msg).into_bytes();
            }
        };

        if result.is_ok() {
            if Self::should_log_to_aof(&cmd) {
                self.append_aof_if_mutating(&cmd).await;
            }
            // For any key a successful mutating command wrote: bump its WATCH
            // version (cheap no-op when un-watched) and emit keyspace
            // notifications. Gated on `mutated_keys` (not AOF) so it also covers
            // zset/bitmap/geo writes; notifications are skipped when disabled.
            let mutated = Self::mutated_keys(&cmd);
            if !mutated.is_empty() {
                for key in &mutated {
                    self.watch_registry.bump(key);
                }
                let flags = self.notify_flags.load(Ordering::Relaxed);
                if flags != 0 {
                    if let Some((class_bit, event)) = Self::keyspace_event(&cmd) {
                        let k_on = flags & NOTIFY_KEYSPACE != 0 && flags & class_bit != 0;
                        let e_on = flags & NOTIFY_KEYEVENT != 0 && flags & class_bit != 0;
                        for key in &mutated {
                            if k_on {
                                self.pubsub
                                    .publish(&format!("__keyspace@0__:{key}"), event.as_bytes());
                            }
                            if e_on {
                                self.pubsub
                                    .publish(&format!("__keyevent@0__:{event}"), key.as_bytes());
                            }
                        }
                    }
                }
            }
        }

        self.wrap_response(ctx, result)
    }

    /// Remove all of a client's channel and pattern subscriptions.
    ///
    /// Called from the connection reader when a client disconnects so the
    /// registry never retains dead push senders.
    pub fn pubsub_cleanup(&self, client_id: uuid::Uuid) {
        self.pubsub.cleanup_client(client_id);
    }

    /// Release a single WATCH version held on `key` (called on disconnect).
    pub fn unwatch_key(&self, key: &str) {
        self.watch_registry.unwatch(key);
    }

    /// MULTI: begin queueing commands. Nested MULTI is an error.
    async fn handle_multi(&self, conn_arc: Arc<Mutex<Connection>>) -> Vec<u8> {
        let txn_arc = conn_arc.lock().await.txn_handle();
        let mut txn = txn_arc.lock().await;
        if txn.queued.is_some() {
            return b"-ERR MULTI calls can not be nested\r\n".to_vec();
        }
        txn.queued = Some(Vec::new());
        b"+OK\r\n".to_vec()
    }

    /// DISCARD: drop the queued transaction and release WATCHes.
    async fn handle_discard(&self, conn_arc: Arc<Mutex<Connection>>) -> Vec<u8> {
        let txn_arc = conn_arc.lock().await.txn_handle();
        let watched = {
            let mut txn = txn_arc.lock().await;
            if txn.queued.is_none() {
                return b"-ERR DISCARD without MULTI\r\n".to_vec();
            }
            txn.queued = None;
            txn.dirty = false;
            std::mem::take(&mut txn.watched)
        };
        for (key, _) in &watched {
            self.watch_registry.unwatch(key);
        }
        b"+OK\r\n".to_vec()
    }

    /// WATCH: record current versions of keys for the CAS check at EXEC. Not
    /// permitted inside MULTI.
    async fn handle_watch(&self, keys: Vec<String>, conn_arc: Arc<Mutex<Connection>>) -> Vec<u8> {
        let txn_arc = conn_arc.lock().await.txn_handle();
        let mut txn = txn_arc.lock().await;
        if txn.queued.is_some() {
            return b"-ERR WATCH inside MULTI is not allowed\r\n".to_vec();
        }
        for key in keys {
            let version = self.watch_registry.watch(&key);
            txn.watched.push((key, version));
        }
        b"+OK\r\n".to_vec()
    }

    /// UNWATCH: forget all watched keys.
    async fn handle_unwatch(&self, conn_arc: Arc<Mutex<Connection>>) -> Vec<u8> {
        let txn_arc = conn_arc.lock().await.txn_handle();
        let watched = {
            let mut txn = txn_arc.lock().await;
            std::mem::take(&mut txn.watched)
        };
        for (key, _) in &watched {
            self.watch_registry.unwatch(key);
        }
        b"+OK\r\n".to_vec()
    }

    /// EXEC: run the queued commands. Guarantee level: each queued command is
    /// individually atomic and applied sequentially; if any WATCHed key changed
    /// since WATCH the whole transaction is abandoned (RESP null array); if a
    /// queued command failed to parse, abort with EXECABORT. This is Redis's
    /// optimistic check-and-set contract (not multi-key linearizability).
    async fn handle_exec(
        &self,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
        conn_arc: Arc<Mutex<Connection>>,
        ctx: ConnCtx,
    ) -> Vec<u8> {
        let txn_arc = conn_arc.lock().await.txn_handle();

        let (queued, dirty, watched) = {
            let mut txn = txn_arc.lock().await;
            if txn.queued.is_none() {
                return b"-ERR EXEC without MULTI\r\n".to_vec();
            }
            let queued = txn.queued.take().unwrap();
            let dirty = txn.dirty;
            txn.dirty = false;
            let watched = std::mem::take(&mut txn.watched);
            (queued, dirty, watched)
        };

        // Check the optimistic CAS BEFORE releasing the WATCHes (unwatch may
        // forget the version once the last watcher leaves).
        let aborted = watched
            .iter()
            .any(|(key, version)| self.watch_registry.current(key) != *version);
        for (key, _) in &watched {
            self.watch_registry.unwatch(key);
        }

        if dirty {
            return b"-EXECABORT Transaction discarded because of previous errors.\r\n".to_vec();
        }
        if aborted {
            return Value::NilArray.encode(ctx.proto);
        }

        // Apply each queued command and assemble a RESP array of their replies.
        let mut out = format!("*{}\r\n", queued.len()).into_bytes();
        for cmd in queued {
            let reply = self
                .execute_inner(cmd, active_connections.clone(), conn_arc.clone(), ctx)
                .await;
            out.extend_from_slice(&reply);
        }
        out
    }

    /// Keys written by a successful mutating command (empty for reads / control
    /// commands). Used to bump WATCH versions. Conservative — covers the
    /// string/number/generic/hash/list/set/json/graph write commands; anything
    /// not listed simply isn't WATCH-tracked.
    /// Update the live keyspace-notification flags from a config string.
    pub fn set_notify_flags(&self, flag_str: &str) {
        self.notify_flags
            .store(parse_notify_flags(flag_str), Ordering::Relaxed);
    }

    /// Map a successful mutating command to its `(class bit, Redis event name)`
    /// for keyspace notifications, or `None` if it produces no event.
    fn keyspace_event(cmd: &Command) -> Option<(u32, &'static str)> {
        use crate::cld_srv::command::Command::*;
        Some(match cmd {
            Set { .. }
            | SetNx { .. }
            | SetEx { .. }
            | PSetEx { .. }
            | GetSet { .. }
            | MSet { .. }
            | MSetNx { .. } => (NK_STRING, "set"),
            Append { .. } => (NK_STRING, "append"),
            SetRange { .. } => (NK_STRING, "setrange"),
            Incr { .. } | IncrBy { .. } => (NK_STRING, "incrby"),
            Decr { .. } | DecrBy { .. } => (NK_STRING, "decrby"),
            IncrByFloat { .. } => (NK_STRING, "incrbyfloat"),
            SetBit { .. } => (NK_STRING, "setbit"),
            BitOp { .. } => (NK_STRING, "set"),
            PfAdd { .. } | PfMerge { .. } => (NK_STRING, "pfadd"),
            GetDel { .. } | Del { .. } | Unlink { .. } => (NK_GENERIC, "del"),
            Expire { .. } | PExpire { .. } | ExpireAt { .. } | PExpireAt { .. } => {
                (NK_GENERIC, "expire")
            }
            Persist { .. } => (NK_GENERIC, "persist"),
            Rename { .. } | RenameNx { .. } => (NK_GENERIC, "rename_to"),
            Copy { .. } => (NK_GENERIC, "copy_to"),
            ListLPush { .. } => (NK_LIST, "lpush"),
            ListRPush { .. } => (NK_LIST, "rpush"),
            LPop { .. } => (NK_LIST, "lpop"),
            RPop { .. } => (NK_LIST, "rpop"),
            LSet { .. } => (NK_LIST, "lset"),
            LInsert { .. } => (NK_LIST, "linsert"),
            LTrim { .. } => (NK_LIST, "ltrim"),
            LRem { .. } => (NK_LIST, "lrem"),
            RPopLPush { .. } => (NK_LIST, "rpop"),
            SAdd { .. } => (NK_SET, "sadd"),
            SRem { .. } => (NK_SET, "srem"),
            SPop { .. } => (NK_SET, "spop"),
            HSet { .. } | HMSet { .. } | HSetNx { .. } => (NK_HASH, "hset"),
            HDel { .. } => (NK_HASH, "hdel"),
            HIncrBy { .. } => (NK_HASH, "hincrby"),
            HIncrByFloat { .. } => (NK_HASH, "hincrbyfloat"),
            ZAdd { .. } | GeoAdd { .. } => (NK_ZSET, "zadd"),
            ZIncrBy { .. } => (NK_ZSET, "zincr"),
            ZRem { .. } => (NK_ZSET, "zrem"),
            ZPopMin { .. } => (NK_ZSET, "zpopmin"),
            ZPopMax { .. } => (NK_ZSET, "zpopmax"),
            ZRemRangeByRank { .. } | ZRemRangeByScore { .. } | ZRemRangeByLex { .. } => {
                (NK_ZSET, "zremrangebyscore")
            }
            XAdd { .. } => (NK_STREAM, "xadd"),
            XDel { .. } => (NK_STREAM, "xdel"),
            XTrim { .. } => (NK_STREAM, "xtrim"),
            XGroupCreate { .. } => (NK_STREAM, "xgroup-create"),
            XGroupSetId { .. } => (NK_STREAM, "xgroup-setid"),
            XGroupDestroy { .. } => (NK_STREAM, "xgroup-destroy"),
            XGroupCreateConsumer { .. } => (NK_STREAM, "xgroup-createconsumer"),
            XGroupDelConsumer { .. } => (NK_STREAM, "xgroup-delconsumer"),
            XAck { .. } => (NK_STREAM, "xack"),
            XClaim { .. } => (NK_STREAM, "xclaim"),
            XAutoClaim { .. } => (NK_STREAM, "xautoclaim"),
            XSetId { .. } => (NK_STREAM, "xsetid"),
            _ => return None,
        })
    }

    fn mutated_keys(cmd: &Command) -> Vec<String> {
        use crate::cld_srv::command::Command::*;
        match cmd {
            Set { key, .. }
            | SetNx { key, .. }
            | SetEx { key, .. }
            | PSetEx { key, .. }
            | GetSet { key, .. }
            | GetDel { key }
            | GetEx { key, .. }
            | Append { key, .. }
            | SetRange { key, .. }
            | Incr { key }
            | Decr { key }
            | IncrBy { key, .. }
            | DecrBy { key, .. }
            | IncrByFloat { key, .. }
            | Expire { key, .. }
            | PExpire { key, .. }
            | ExpireAt { key, .. }
            | PExpireAt { key, .. }
            | Persist { key }
            | SAdd { key, .. }
            | SRem { key, .. }
            | SPop { key, .. }
            | HSet { key, .. }
            | HMSet { key, .. }
            | HSetNx { key, .. }
            | HDel { key, .. }
            | HIncrBy { key, .. }
            | HIncrByFloat { key, .. }
            | ListLPush { key, .. }
            | ListRPush { key, .. }
            | LPop { key }
            | RPop { key }
            | LSet { key, .. }
            | LInsert { key, .. }
            | LTrim { key, .. }
            | LRem { key, .. }
            | JsonSet { key, .. }
            | JsonDel { key, .. }
            | JsonNumIncrBy { key, .. }
            | JsonStrAppend { key, .. }
            | JsonArrAppend { key, .. }
            | JsonObjSet { key, .. }
            | JsonArrInsert { key, .. }
            | JsonArrPop { key, .. }
            | JsonArrTrim { key, .. }
            | ZAdd { key, .. }
            | ZIncrBy { key, .. }
            | ZRem { key, .. }
            | ZPopMin { key, .. }
            | ZPopMax { key, .. }
            | ZRemRangeByRank { key, .. }
            | ZRemRangeByScore { key, .. }
            | ZRemRangeByLex { key, .. }
            | SetBit { key, .. }
            | PfAdd { key, .. }
            | GeoAdd { key, .. }
            | XAdd { key, .. }
            | XDel { key, .. }
            | XTrim { key, .. }
            | XGroupCreate { key, .. }
            | XGroupSetId { key, .. }
            | XGroupDestroy { key, .. }
            | XGroupCreateConsumer { key, .. }
            | XGroupDelConsumer { key, .. }
            | XAck { key, .. }
            | XClaim { key, .. }
            | XAutoClaim { key, .. }
            | XSetId { key, .. } => vec![key.clone()],
            XReadGroup { streams, .. } => streams.iter().map(|(k, _)| k.clone()).collect(),
            BitOp { dest, .. } | PfMerge { dest, .. } => vec![dest.clone()],
            Del { keys } | Unlink { keys } => keys.clone(),
            MSet { key_values } | MSetNx { key_values } => {
                key_values.iter().map(|(k, _)| k.clone()).collect()
            }
            Rename { key, new_key } | RenameNx { key, new_key } => {
                vec![key.clone(), new_key.clone()]
            }
            Copy { destination, .. } => vec![destination.clone()],
            Move { key, .. } => vec![key.clone()],
            RPopLPush {
                source,
                destination,
            } => vec![source.clone(), destination.clone()],
            BLPop { keys, .. } | BRPop { keys, .. } => keys.clone(),
            _ => Vec::new(),
        }
    }

    /// Handle `SUBSCRIBE`/`PSUBSCRIBE` (`is_pattern` selects which).
    ///
    /// Redis emits one confirmation frame per channel/pattern, in order, each
    /// carrying the client's running subscription total. We snapshot the
    /// connection's identity (client id, push sender, protocol) under a single
    /// brief lock, register every name against the registry (which returns the
    /// updated total per call), then commit only the final total back onto the
    /// connection.
    async fn handle_subscribe(
        &self,
        names: Vec<String>,
        is_pattern: bool,
        ctx: ConnCtx,
        conn_arc: Arc<Mutex<Connection>>,
    ) -> Vec<u8> {
        let (client, push_tx, proto) = {
            let conn = conn_arc.lock().await;
            (conn.get_client_id(), conn.push_sender(), conn.protocol())
        };

        let kind: &[u8] = if is_pattern {
            b"psubscribe"
        } else {
            b"subscribe"
        };

        let mut out = Vec::new();
        let mut final_total = None;
        for name in names {
            let new_total = if is_pattern {
                self.pubsub
                    .subscribe_pattern(client, &push_tx, proto, &name)
            } else {
                self.pubsub
                    .subscribe_channel(client, &push_tx, proto, &name)
            };
            final_total = Some(new_total);
            let frame = Value::Push(vec![
                Value::BulkString(kind.to_vec()),
                Value::BulkString(name.into_bytes()),
                Value::Integer(new_total as i64),
            ]);
            out.extend_from_slice(&frame.encode(ctx.proto));
        }

        if let Some(total) = final_total {
            let conn = conn_arc.lock().await;
            conn.set_subscription_total(total);
        }

        out
    }

    /// Handle `UNSUBSCRIBE`/`PUNSUBSCRIBE` (`is_pattern` selects which).
    ///
    /// An empty `names` list means "unsubscribe from everything", so we expand it
    /// to the client's currently-registered channels/patterns. If the client had
    /// nothing subscribed, Redis still sends a single confirmation frame with a
    /// nil name and the (unchanged) total.
    async fn handle_unsubscribe(
        &self,
        names: Vec<String>,
        is_pattern: bool,
        ctx: ConnCtx,
        conn_arc: Arc<Mutex<Connection>>,
    ) -> Vec<u8> {
        let client = {
            let conn = conn_arc.lock().await;
            conn.get_client_id()
        };

        let kind: &[u8] = if is_pattern {
            b"punsubscribe"
        } else {
            b"unsubscribe"
        };

        let targets = if names.is_empty() {
            if is_pattern {
                self.pubsub.client_patterns(client)
            } else {
                self.pubsub.client_channels(client)
            }
        } else {
            names
        };

        if targets.is_empty() {
            let total = self.pubsub.total_for_client(client);
            let frame = Value::Push(vec![
                Value::BulkString(kind.to_vec()),
                Value::Nil,
                Value::Integer(total as i64),
            ]);
            let conn = conn_arc.lock().await;
            conn.set_subscription_total(total);
            return frame.encode(ctx.proto);
        }

        let mut out = Vec::new();
        let mut final_total = None;
        for name in targets {
            let new_total = if is_pattern {
                self.pubsub.unsubscribe_pattern(client, &name)
            } else {
                self.pubsub.unsubscribe_channel(client, &name)
            };
            final_total = Some(new_total);
            let frame = Value::Push(vec![
                Value::BulkString(kind.to_vec()),
                Value::BulkString(name.into_bytes()),
                Value::Integer(new_total as i64),
            ]);
            out.extend_from_slice(&frame.encode(ctx.proto));
        }

        if let Some(total) = final_total {
            let conn = conn_arc.lock().await;
            conn.set_subscription_total(total);
        }

        out
    }

    /// Handle `PUBLISH channel message`; returns the number of receivers reached.
    fn handle_publish(&self, channel: &str, message: &[u8], ctx: ConnCtx) -> Vec<u8> {
        let n = self.pubsub.publish(channel, message);
        self.wrap_response(ctx, Ok(DataType::Integer(n as i64)))
    }

    /// Handle `PUBSUB CHANNELS [pattern]`; returns the list of active channels.
    fn handle_pubsub_channels(&self, pattern: Option<&str>, ctx: ConnCtx) -> Vec<u8> {
        Value::Array(
            self.pubsub
                .channels_list(pattern)
                .into_iter()
                .map(|c| Value::BulkString(c.into_bytes()))
                .collect(),
        )
        .encode(ctx.proto)
    }

    /// Handle `PUBSUB NUMSUB [channel ...]`; returns a flat
    /// `[name, count, name, count, ...]` array.
    fn handle_pubsub_numsub(&self, names: &[String], ctx: ConnCtx) -> Vec<u8> {
        let mut items = Vec::new();
        for (name, count) in self.pubsub.numsub(names) {
            items.push(Value::BulkString(name.into_bytes()));
            items.push(Value::Integer(count as i64));
        }
        Value::Array(items).encode(ctx.proto)
    }

    /// Handle `PUBSUB NUMPAT`; returns the number of unique patterns.
    fn handle_pubsub_numpat(&self, ctx: ConnCtx) -> Vec<u8> {
        self.wrap_response(ctx, Ok(DataType::Integer(self.pubsub.numpat() as i64)))
    }

    /// Handle `HELLO [protover] [AUTH <user> <pass>] [SETNAME <name>]`.
    ///
    /// Authentication and the no-auth `NOAUTH` gating for HELLO are performed
    /// earlier in the connection reader (alongside `AUTH`), so by the time HELLO
    /// reaches here the client is authorized. This commits the negotiated
    /// protocol version (and optional client name) onto the shared connection
    /// state and returns the server handshake map encoded in that protocol — so
    /// a client that requested RESP3 immediately receives a `%` map.
    async fn handle_hello(
        &self,
        protover: Option<u8>,
        _auth: Option<(String, String)>,
        setname: Option<String>,
        ctx: ConnCtx,
        current_connection_arc: Arc<Mutex<Connection>>,
    ) -> Vec<u8> {
        // No explicit version keeps the connection's current protocol.
        let requested = protover.unwrap_or(ctx.proto.as_u8());
        let proto = match ProtocolVersion::from_u8(requested) {
            Some(p) => p,
            None => return b"-NOPROTO unsupported protocol version\r\n".to_vec(),
        };

        // Commit protocol + optional client name on the shared connection state.
        // The protocol atomic is shared with the writer's clone, so the next
        // command's `ctx()` snapshot already reflects this switch. We clone the
        // connection handle out of the guard and drop the guard before awaiting
        // `set_client_name`, so the connection mutex is never held across an
        // await (the clone shares the same inner name slot via `Arc`).
        let conn = {
            let guard = current_connection_arc.lock().await;
            guard.set_protocol(proto);
            guard.clone()
        };
        if let Some(name) = setname {
            conn.set_client_name(name).await;
        }

        let bulk = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        let map = Value::Map(vec![
            (bulk("server"), bulk("calod")),
            (bulk("version"), bulk("0.1.0")),
            (bulk("proto"), Value::Integer(proto.as_u8() as i64)),
            (bulk("id"), Value::Integer(1)),
            (bulk("mode"), bulk("standalone")),
            (bulk("role"), bulk("master")),
            (bulk("modules"), Value::Array(vec![])),
        ]);
        map.encode(proto)
    }

    /// Handle `CONFIG GET|SET|RESETSTAT|REWRITE`. GET returns a map (flat array
    /// in RESP2, `%` in RESP3) of resolved parameter values; unknown params are
    /// simply omitted (never an error) so client probes succeed. SET validates
    /// each parameter against the registry.
    fn handle_config(&self, sub: ConfigSubCommand, ctx: ConnCtx) -> Vec<u8> {
        match sub {
            ConfigSubCommand::Get { params } => {
                let cfg = self.store.config();
                let mut pairs: Vec<(Value, Value)> = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for pat in &params {
                    for (k, v) in config_get(cfg, &self.config_overlay, pat) {
                        if seen.insert(k.clone()) {
                            pairs.push((
                                Value::BulkString(k.into_bytes()),
                                Value::BulkString(v.into_bytes()),
                            ));
                        }
                    }
                }
                Value::Map(pairs).encode(ctx.proto)
            }
            ConfigSubCommand::Set { pairs } => {
                for (k, v) in &pairs {
                    if let Err(e) = config_set(&self.config_overlay, k, v) {
                        return format!("-ERR {}\r\n", e).into_bytes();
                    }
                    // Apply live side effects for params with runtime behavior.
                    if k.eq_ignore_ascii_case("notify-keyspace-events") {
                        self.set_notify_flags(v);
                    }
                }
                self.static_response(OK_RESPONSE)
            }
            ConfigSubCommand::ResetStat | ConfigSubCommand::Rewrite => {
                self.static_response(OK_RESPONSE)
            }
        }
    }

    async fn handle_client_command(
        &self,
        subcommand: ClientSubCommand,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
        current_connection_arc: Arc<Mutex<Connection>>,
        ctx: ConnCtx,
    ) -> Vec<u8> {
        match subcommand {
            ClientSubCommand::List => {
                let client_list_info = self.get_client_list_info(active_connections).await;
                self.wrap_response(ctx, Ok(DataType::string_from(client_list_info)))
            }
            ClientSubCommand::Info => {
                let client_info = self.get_client_conn_info(current_connection_arc).await;
                self.wrap_response(ctx, Ok(DataType::string_from(client_info)))
            }
            ClientSubCommand::GetName => {
                let client_name = self.get_client_name(current_connection_arc).await;
                self.wrap_response(ctx, Ok(DataType::string_from(client_name)))
            }
            ClientSubCommand::SetName { connection_name } => {
                self.set_client_name(current_connection_arc, connection_name)
                    .await;
                self.static_response(OK_RESPONSE)
            }
            ClientSubCommand::Kill { ip_port } => {
                let kill_result = self
                    .kill_client_connection(ip_port, active_connections)
                    .await;
                if kill_result {
                    self.static_response(OK_RESPONSE)
                } else {
                    let err_msg = "-ERR No client found matching address\r\n";
                    err_msg.as_bytes().to_vec()
                }
            }
            ClientSubCommand::Id => {
                let id = {
                    let conn = current_connection_arc.lock().await;
                    conn.numeric_id()
                };
                self.wrap_response(ctx, Ok(DataType::Integer(id as i64)))
            }
            // SETINFO/NO-EVICT/NO-TOUCH/UNPAUSE are accepted as no-ops (clients
            // set lib-name/lib-ver and eviction hints on connect); reply OK.
            ClientSubCommand::SetInfo
            | ClientSubCommand::NoEvict
            | ClientSubCommand::NoTouch
            | ClientSubCommand::Unpause => self.static_response(OK_RESPONSE),
            ClientSubCommand::NoSubCommand => self.static_response(OK_RESPONSE),
        }
    }

    /// Client list info generation
    pub async fn get_client_list_info(
        &self,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
    ) -> String {
        let conn_count = active_connections.len();
        let estimated_size = conn_count * 128;
        let mut output = String::with_capacity(estimated_size);

        for entry in active_connections.iter() {
            if let Some(conn_arc) = entry.value().upgrade() {
                let client_info = {
                    let conn = conn_arc.lock().await;
                    let client_id = conn.get_client_id();
                    let peer_addr = conn.get_peer_address().ok();
                    let name = conn.get_client_name().await.unwrap_or_default();
                    (client_id, peer_addr, name)
                };

                // format the string
                if let Some(peer_addr) = client_info.1 {
                    let (client_id, _, name) = client_info;
                    output.push_str(&format!(
                        "id={} addr={} name={} age=0 idle=0 flags=N db=0 sub=0 psub=0 multi=- qbuf=0 qbuf-free=0 obl=0 oll=0 omem=0 events=r cmd=unknown client-type=normal\n",
                        client_id, peer_addr, name,
                    ));
                }
            }
        }
        output
    }

    /// Client connection info retrieval
    async fn get_client_conn_info(&self, current_connection_arc: Arc<Mutex<Connection>>) -> String {
        let client_data = {
            let conn = current_connection_arc.lock().await;
            let client_id = conn.get_client_id();
            let peer_addr = conn.get_peer_address().ok();
            let name = conn.get_client_name().await.unwrap_or_default();
            (client_id, peer_addr, name)
        };

        // Format response
        let mut output = String::with_capacity(256);
        if let Some(peer_addr) = client_data.1 {
            let (client_id, _, name) = client_data;
            output.push_str(&format!(
                "id:{}\r\naddr:{}\r\nname:{}\r\nage:0\r\nidle:0\r\nflags:N\r\ndb:0\r\nsub:0\r\npsub:0\r\nmulti:-1\r\nqbuf:0\r\nqbuf-free:0\r\nobl:0\r\noll:0\r\nomem:0\r\nevents:r\r\ncmd:unknown\r\nuser:default\r\nclient-type:normal\r\nname:{}\r\n",
                client_id, peer_addr, name, name,
            ));
        }
        output
    }

    /// Get client name with minimal locking
    pub async fn get_client_name(&self, current_connection_arc: Arc<Mutex<Connection>>) -> String {
        let conn = current_connection_arc.lock().await;
        conn.get_client_name().await.unwrap_or_default()
    }

    /// Set client name with minimal locking
    pub async fn set_client_name(
        &self,
        current_connection_arc: Arc<Mutex<Connection>>,
        connection_name: String,
    ) {
        let conn = current_connection_arc.lock().await;
        conn.set_client_name(connection_name).await;
    }

    /// Kill a client connection with efficient lookup
    pub async fn kill_client_connection(
        &self,
        ip_port_str: String,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
    ) -> bool {
        // Fast path - parse address once outside the loop
        if let Ok((target_ip, target_port)) = parse_ip_port(&ip_port_str) {
            // Scan connections without fully acquiring each one
            for entry in active_connections.iter() {
                if let Some(conn_arc) = entry.value().upgrade() {
                    let should_kill = {
                        // Briefly lock just to get address
                        let conn = conn_arc.lock().await;
                        if let Ok(peer_addr) = conn.get_peer_address() {
                            peer_addr.ip() == target_ip && peer_addr.port() == target_port
                        } else {
                            false
                        }
                    };

                    if should_kill {
                        info!("Killing client connection: {}", ip_port_str);
                        let conn = conn_arc.lock().await;
                        conn.mark_closing().await;
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Generate the `INFO [section]` reply (a single RESP bulk string).
    ///
    /// Redis semantics: with no argument (or `default`) the standard sections are
    /// returned; `all`/`everything` adds every section; a specific section name
    /// returns ONLY that section. Each section is emitted under its canonical
    /// `# Name` header and separated by a blank line.
    pub async fn info(&self, section: Option<&str>) -> Vec<u8> {
        let mut report = String::with_capacity(4096);
        let cfg = self.store.config();
        let uptime_secs = self.start_time.elapsed().as_secs();

        let sec_lower = section.map(|s| s.to_lowercase());
        // Whether a given section should be included for this request.
        let want = |name: &str| match sec_lower.as_deref() {
            // `default` (and the no-arg case) omits the verbose commandstats.
            None | Some("default") => name != "commandstats",
            Some("all") | Some("everything") => true,
            Some(requested) => requested == name,
        };

        if want("server") {
            report.push_str("# Server\r\n");
            // Many clients gate feature availability on `redis_version`.
            report.push_str("redis_version:7.4.0\r\n");
            report.push_str("calod_version:1.0.0\r\n");
            report.push_str("redis_mode:standalone\r\n");
            report.push_str(&format!("run_id:{}\r\n", self.run_id));
            report.push_str(&format!("tcp_port:{}\r\n", cfg.listen_addr.port()));
            report.push_str(&format!("uptime_in_seconds:{uptime_secs}\r\n"));
            report.push_str(&format!("uptime_in_days:{}\r\n", uptime_secs / 86400));
            report.push_str("hz:10\r\n");
            report.push_str("os:unknown\r\n");
            report.push_str("arch_bits:64\r\n");
            report.push_str("process_id:0\r\n");
            report.push_str("executable:/path/to/calod\r\n");
            report.push_str("config_file:\r\n");
            report.push_str("\r\n");
        }

        if want("clients") {
            report.push_str("# Clients\r\n");
            report.push_str(&format!(
                "connected_clients:{}\r\n",
                self.metrics.get_current_connections()
            ));
            report.push_str("client_recent_max_input_buffer:0\r\n");
            report.push_str("client_recent_max_output_buffer:0\r\n");
            report.push_str("blocked_clients:0\r\n");
            report.push_str("\r\n");
        }

        if want("memory") {
            report.push_str("# Memory\r\n");
            let used_memory = self.store.total_data_size();
            report.push_str(&format!("used_memory:{used_memory}\r\n"));
            report.push_str(&format!(
                "used_memory_human:{}\r\n",
                human_bytes(used_memory)
            ));
            report.push_str(&format!("used_memory_peak:{used_memory}\r\n"));
            report.push_str(&format!(
                "used_memory_peak_human:{}\r\n",
                human_bytes(used_memory)
            ));
            report.push_str(&format!("used_memory_dataset:{used_memory}\r\n"));
            report.push_str(&format!(
                "used_memory_dataset_human:{}\r\n",
                human_bytes(used_memory)
            ));
            // Reflect maxmemory / policy from config + any CONFIG SET overrides,
            // so INFO and CONFIG GET stay consistent.
            let maxmemory = config_get(cfg, &self.config_overlay, "maxmemory")
                .into_iter()
                .next()
                .map(|(_, v)| v)
                .unwrap_or_else(|| "0".to_string());
            let maxmemory_policy = config_get(cfg, &self.config_overlay, "maxmemory-policy")
                .into_iter()
                .next()
                .map(|(_, v)| v)
                .unwrap_or_else(|| "noeviction".to_string());
            report.push_str(&format!("maxmemory:{maxmemory}\r\n"));
            report.push_str(&format!(
                "maxmemory_human:{}\r\n",
                human_bytes(maxmemory.parse().unwrap_or(0))
            ));
            report.push_str(&format!("maxmemory_policy:{maxmemory_policy}\r\n"));
            report.push_str("allocator:jemalloc\r\n");
            report.push_str("active_defrag_running:0\r\n");
            report.push_str("lazyfree_pending_objects:0\r\n");
            report.push_str("\r\n");
        }

        if want("persistence") {
            report.push_str("# Persistence\r\n");
            report.push_str("loading:0\r\n");
            report.push_str("rdb_changes_since_last_save:0\r\n");
            report.push_str("rdb_bgsave_in_progress:0\r\n");
            report.push_str("rdb_last_save_time:0\r\n");
            report.push_str("aof_enabled:0\r\n");
            report.push_str("aof_rewrite_in_progress:0\r\n");
            report.push_str("aof_rewrite_scheduled:0\r\n");
            report.push_str("\r\n");
        }

        if want("stats") {
            report.push_str("# Stats\r\n");
            report.push_str(&self.store.metrics());
            report.push_str(&format!(
                "total_connections_received:{}\r\n",
                self.metrics.get_total_connections()
            ));
            report.push_str(&format!(
                "total_commands_processed:{}\r\n",
                self.metrics.get_total_commands_processed()
            ));
            report.push_str("instantaneous_ops_per_sec:0\r\n");
            report.push_str("instantaneous_input_kbps:0.00\r\n");
            report.push_str("instantaneous_output_kbps:0.00\r\n");
            report.push_str("rejected_connections:0\r\n");
            report.push_str("expired_keys:0\r\n");
            report.push_str("evicted_keys:0\r\n");
            report.push_str("keyspace_hits:0\r\n");
            report.push_str("keyspace_misses:0\r\n");
            report.push_str("\r\n");
        }

        if want("replication") {
            report.push_str("# Replication\r\n");
            report.push_str("role:master\r\n");
            report.push_str("connected_slaves:0\r\n");
            report.push_str("master_replid:8371b4fb1155914e2fd21aa0e9adc5c0d821e821\r\n");
            report.push_str("master_replid2:0000000000000000000000000000000000000000\r\n");
            report.push_str("master_repl_offset:0\r\n");
            report.push_str("second_repl_offset:-1\r\n");
            report.push_str("repl_backlog_active:0\r\n");
            report.push_str("repl_backlog_size:1048576\r\n");
            report.push_str("repl_backlog_first_byte_offset:0\r\n");
            report.push_str("repl_backlog_histlen:0\r\n");
            report.push_str("\r\n");
        }

        if want("cpu") {
            report.push_str("# CPU\r\n");
            report.push_str("used_cpu_sys:0.0\r\n");
            report.push_str("used_cpu_user:0.0\r\n");
            report.push_str("used_cpu_sys_children:0.0\r\n");
            report.push_str("used_cpu_user_children:0.0\r\n");
            report.push_str("\r\n");
        }

        if want("commandstats") {
            report.push_str("# Commandstats\r\n");
            report.push_str("cmdstat_ping:calls=0,usec=0,usec_per_call=0.00\r\n");
            report.push_str("cmdstat_get:calls=0,usec=0,usec_per_call=0.00\r\n");
            report.push_str("cmdstat_set:calls=0,usec=0,usec_per_call=0.00\r\n");
            report.push_str("\r\n");
        }

        if want("cluster") {
            report.push_str("# Cluster\r\n");
            report.push_str("cluster_enabled:0\r\n");
            report.push_str("\r\n");
        }

        if want("keyspace") {
            report.push_str("# Keyspace\r\n");
            let dbsize = match self.store.dbsize_cmd().await {
                Ok(DataType::Integer(n)) if n > 0 => n,
                _ => 0,
            };
            if dbsize > 0 {
                report.push_str(&format!("db0:keys={dbsize},expires=0,avg_ttl=0\r\n"));
            }
            report.push_str("\r\n");
        }

        // RESP bulk string framing; length is the byte count of `report`.
        let resp = format!("${}\r\n{}\r\n", report.len(), report);
        debug!("INFO response generated, section: {:?}", section);
        resp.into_bytes()
    }

    /// Precompute COMMAND response for better performance
    fn precompute_command_resp() -> Vec<u8> {
        let commands = Self::get_command_resp();
        let mut buffer = BytesMut::with_capacity(commands.len() * 20); // Reasonable estimate

        // Format as Redis array
        buffer.put_u8(b'*');
        buffer.extend_from_slice(commands.len().to_string().as_bytes());
        buffer.extend_from_slice(b"\r\n");

        // Add each command entry
        for cmd in &commands {
            buffer.put_u8(b'$');
            buffer.extend_from_slice(cmd.len().to_string().as_bytes());
            buffer.extend_from_slice(b"\r\n");
            buffer.extend_from_slice(cmd.as_bytes());
            buffer.extend_from_slice(b"\r\n");
        }

        buffer.to_vec()
    }

    /// Precompute COMMAND LIST response
    fn precompute_command_list_resp() -> Vec<u8> {
        let commands = Self::get_command_list_resp();
        let mut buffer = BytesMut::with_capacity(commands.len() * 15); // Reasonable estimate

        // Format as Redis array
        buffer.put_u8(b'*');
        buffer.extend_from_slice(commands.len().to_string().as_bytes());
        buffer.extend_from_slice(b"\r\n");

        // Add each command name
        for cmd in &commands {
            buffer.put_u8(b'$');
            buffer.extend_from_slice(cmd.len().to_string().as_bytes());
            buffer.extend_from_slice(b"\r\n");
            buffer.extend_from_slice(cmd.as_bytes());
            buffer.extend_from_slice(b"\r\n");
        }

        buffer.to_vec()
    }

    // Redis compatibility: COMMAND response format
    fn get_command_resp() -> Vec<String> {
        // Return RESP array for COMMAND command (list of all commands and arities)
        // Format matches Redis command info structure
        Vec::<String>::from([
            String::from("PING"),
            String::from("-1"), // Arity -1 for variable arguments or -N for commands taking exactly N arguments
            String::from("readonly,fast"), // Command flags for Redis compatibility
            String::from("0"),  // First key position (0-based or -1 if none)
            String::from("0"),  // Last key position
            String::from("1"),  // Step count for locating keys
            String::from("QUIT"),
            String::from("1"),
            String::from("noscript,fast"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            String::from("HELLO"),
            String::from("-1"),
            String::from("noscript,loading,stale"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            String::from("ECHO"),
            String::from("2"), // Arity 2 (including command name and one argument)
            String::from("readonly,fast"),
            String::from("-1"), // No key
            String::from("-1"),
            String::from("0"),
            String::from("SET"),
            String::from("-3"), // Arity -3 (including command name, key, value, and optional expire/NX/XX)
            String::from("write,denyoom"),
            String::from("1"), // Key at position 1
            String::from("1"),
            String::from("1"),
            String::from("SETNX"),
            String::from("3"),
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("SETEX"),
            String::from("4"),
            String::from("write,denyoom"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("PSETEX"),
            String::from("4"),
            String::from("write,denyoom"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("GET"),
            String::from("2"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("GETDEL"),
            String::from("2"),
            String::from("write,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("GETEX"),
            String::from("-2"),
            String::from("write,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("MGET"),
            String::from("-2"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("-1"),
            String::from("1"),
            String::from("MSET"),
            String::from("-3"),
            String::from("write"),
            String::from("1"),
            String::from("-1"),
            String::from("2"),
            String::from("MSETNX"),
            String::from("-3"),
            String::from("write"),
            String::from("1"),
            String::from("-1"),
            String::from("2"),
            String::from("SADD"),
            String::from("-3"),
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("SREM"),
            String::from("-3"),
            String::from("write,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("SMEMBERS"),
            String::from("2"),
            String::from("readonly,sort_for_script"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("SISMEMBER"),
            String::from("3"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("SCARD"),
            String::from("2"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("SPOP"),
            String::from("-2"),
            String::from("write,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("SRANDMEMBER"),
            String::from("-2"),
            String::from("readonly,random"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("EXISTS"),
            String::from("-2"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("-1"),
            String::from("1"),
            String::from("DEL"),
            String::from("-2"),
            String::from("write"),
            String::from("1"),
            String::from("-1"),
            String::from("1"),
            String::from("UNLINK"),
            String::from("-2"),
            String::from("write"),
            String::from("1"),
            String::from("-1"),
            String::from("1"),
            String::from("TOUCH"),
            String::from("-2"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("-1"),
            String::from("1"),
            String::from("KEYS"),
            String::from("2"),
            String::from("readonly,sort_for_script"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            String::from("RANDOMKEY"),
            String::from("1"),
            String::from("readonly"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            String::from("RENAME"),
            String::from("3"),
            String::from("write"),
            String::from("1"),
            String::from("2"),
            String::from("1"),
            String::from("RENAMENX"),
            String::from("3"),
            String::from("write"),
            String::from("1"),
            String::from("2"),
            String::from("1"),
            String::from("COPY"),
            String::from("-3"),
            String::from("write"),
            String::from("1"),
            String::from("2"),
            String::from("1"),
            String::from("MOVE"),
            String::from("3"),
            String::from("write"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("EXPIRE"),
            String::from("3"),
            String::from("write"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("PEXPIRE"),
            String::from("3"),
            String::from("write"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("EXPIREAT"),
            String::from("3"),
            String::from("write"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("PEXPIREAT"),
            String::from("3"),
            String::from("write"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("TTL"),
            String::from("2"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("PTTL"),
            String::from("2"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("EXPIRETIME"),
            String::from("2"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("PEXPIRETIME"),
            String::from("2"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("PERSIST"),
            String::from("2"),
            String::from("write"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("OBJECT"),
            String::from("3"),
            String::from("readonly"),
            String::from("2"),
            String::from("2"),
            String::from("1"),
            String::from("DBSIZE"),
            String::from("1"),
            String::from("readonly"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            String::from("FLUSHDB"),
            String::from("1"),
            String::from("write"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            String::from("FLUSHALL"),
            String::from("1"),
            String::from("write"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            String::from("SELECT"),
            String::from("2"),
            String::from("fast"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            String::from("SCAN"),
            String::from("-2"),
            String::from("readonly"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            String::from("WAIT"),
            String::from("3"),
            String::from("noscript"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            String::from("INCR"),
            String::from("2"),
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("DECR"),
            String::from("2"),
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("INCRBY"),
            String::from("3"),
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("DECRBY"),
            String::from("3"),
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("INCRBYFLOAT"),
            String::from("3"),
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            // Hash commands
            String::from("HSET"),
            String::from("-3"), // Variable arity for HSET key field value [field value ...]
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("HGET"),
            String::from("3"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("HDEL"),
            String::from("-3"), // Variable arity for HDEL key field [field ...]
            String::from("write,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("HEXISTS"),
            String::from("3"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("HGETALL"),
            String::from("2"),
            String::from("readonly"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            // List commands
            String::from("LPUSH"),
            String::from("-3"), // Variable arity for LPUSH key value [value ...]
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("RPUSH"),
            String::from("-3"), // Variable arity for RPUSH key value [value ...]
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("LPOP"),
            String::from("2"),
            String::from("write,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("RPOP"),
            String::from("2"),
            String::from("write,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("BLPOP"),
            String::from("-3"), // Variable arity for BLPOP key [key ...] timeout
            String::from("write,noscript,blocking"),
            String::from("1"),
            String::from("-2"),
            String::from("1"),
            String::from("BRPOP"),
            String::from("-3"), // Variable arity for BRPOP key [key ...] timeout
            String::from("write,noscript,blocking"),
            String::from("1"),
            String::from("-2"),
            String::from("1"),
            // JSON commands with Redis-like metadata
            String::from("JSON.SET"),
            String::from("4"),
            String::from("write,denyoom"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            String::from("JSON.GET"),
            String::from("3"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            // Admin commands
            String::from("COMMAND"),
            String::from("-1"), // Arity -1 for variable arguments
            String::from("readonly,loading,stale"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            String::from("INFO"),
            String::from("-1"), // Arity -1 for variable arguments
            String::from("readonly,loading,stale"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            String::from("CLIENT"),
            String::from("-2"), // Arity -2 for CLIENT subcommands
            String::from("admin,noscript,loading,stale"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
        ])
    }

    fn get_command_list_resp() -> Vec<String> {
        // Return RESP array for COMMAND LIST (list of command names)
        // Redis clients expect lowercase command names
        Vec::<String>::from([
            String::from("ping"),
            String::from("quit"),
            String::from("hello"),
            String::from("echo"),
            String::from("set"),
            String::from("mset"),
            String::from("setex"),
            String::from("psetex"),
            String::from("get"),
            String::from("mget"),
            String::from("exists"),
            String::from("del"),
            String::from("unlink"),
            String::from("touch"),
            String::from("keys"),
            String::from("randomkey"),
            String::from("rename"),
            String::from("renamenx"),
            String::from("copy"),
            String::from("move"),
            String::from("expire"),
            String::from("pexpire"),
            String::from("expireat"),
            String::from("pexpireat"),
            String::from("ttl"),
            String::from("pttl"),
            String::from("expiretime"),
            String::from("pexpiretime"),
            String::from("persist"),
            String::from("object"),
            String::from("dbsize"),
            String::from("flushdb"),
            String::from("flushall"),
            String::from("select"),
            String::from("scan"),
            String::from("wait"),
            String::from("setnx"),
            String::from("getdel"),
            String::from("getex"),
            String::from("msetnx"),
            String::from("append"),
            String::from("getset"),
            String::from("sadd"),
            String::from("srem"),
            String::from("smembers"),
            String::from("sismember"),
            String::from("scard"),
            String::from("spop"),
            String::from("srandmember"),
            String::from("incr"),
            String::from("decr"),
            String::from("incrby"),
            String::from("decrby"),
            String::from("incrbyfloat"),
            String::from("hset"),
            String::from("hget"),
            String::from("hdel"),
            String::from("hexists"),
            String::from("hgetall"),
            String::from("hincrby"),
            String::from("hincrbyfloat"),
            String::from("hkeys"),
            String::from("hlen"),
            String::from("hmget"),
            String::from("hmset"),
            String::from("hsetnx"),
            String::from("hvals"),
            String::from("lpush"),
            String::from("rpush"),
            String::from("lpop"),
            String::from("rpop"),
            String::from("llen"),
            String::from("lrange"),
            String::from("lindex"),
            String::from("linsert"),
            String::from("lset"),
            String::from("ltrim"),
            String::from("lrem"),
            String::from("rpoplpush"),
            String::from("blpop"),
            String::from("brpop"),
            String::from("type"),
            String::from("json.set"),
            String::from("json.get"),
            String::from("json.del"),
            String::from("json.type"),
            String::from("json.numincrby"),
            String::from("json.arrappend"),
            String::from("json.arrlen"),
            String::from("command"),
            String::from("info"),
            String::from("client"),
        ])
    }

    fn get_command_info_resp(command_name: &str) -> Vec<String> {
        // Return RESP array for COMMAND INFO <command_name> (command details)
        match command_name.to_uppercase().as_str() {
            "PING" => Vec::<String>::from([
                String::from("name"),
                String::from("ping"),
                String::from("arity"),
                String::from("-1"),
                String::from("flags"),
                String::from("readonly,fast"), // Example flags
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("1"),
            ]),
            "QUIT" => Vec::<String>::from([
                String::from("name"),
                String::from("quit"),
                String::from("arity"),
                String::from("1"),
                String::from("flags"),
                String::from("noscript,fast"),
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("0"),
            ]),
            "HELLO" => Vec::<String>::from([
                String::from("name"),
                String::from("hello"),
                String::from("arity"),
                String::from("-1"),
                String::from("flags"),
                String::from("noscript,loading,stale"),
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("0"),
            ]),
            "SET" => Vec::<String>::from([
                String::from("name"),
                String::from("set"),
                String::from("arity"),
                String::from("-3"),
                String::from("flags"),
                String::from("write,denyoom"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "SETNX" => Vec::<String>::from([
                String::from("name"),
                String::from("setnx"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("write,denyoom,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "SETEX" => Vec::<String>::from([
                String::from("name"),
                String::from("setex"),
                String::from("arity"),
                String::from("4"),
                String::from("flags"),
                String::from("write,denyoom"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "PSETEX" => Vec::<String>::from([
                String::from("name"),
                String::from("psetex"),
                String::from("arity"),
                String::from("4"),
                String::from("flags"),
                String::from("write,denyoom"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "GET" => Vec::<String>::from([
                String::from("name"),
                String::from("get"),
                String::from("arity"),
                String::from("2"),
                String::from("flags"),
                String::from("readonly,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "GETDEL" => Vec::<String>::from([
                String::from("name"),
                String::from("getdel"),
                String::from("arity"),
                String::from("2"),
                String::from("flags"),
                String::from("write,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "GETEX" => Vec::<String>::from([
                String::from("name"),
                String::from("getex"),
                String::from("arity"),
                String::from("-2"),
                String::from("flags"),
                String::from("write,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "MGET" => Vec::<String>::from([
                String::from("name"),
                String::from("mget"),
                String::from("arity"),
                String::from("-2"),
                String::from("flags"),
                String::from("readonly,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("-1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "GETSET" => Vec::<String>::from([
                String::from("name"),
                String::from("getset"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("write,denyoom"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "MSET" => Vec::<String>::from([
                String::from("name"),
                String::from("mset"),
                String::from("arity"),
                String::from("-3"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("-1"),
                String::from("steps"),
                String::from("2"),
            ]),
            "MSETNX" => Vec::<String>::from([
                String::from("name"),
                String::from("msetnx"),
                String::from("arity"),
                String::from("-3"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("-1"),
                String::from("steps"),
                String::from("2"),
            ]),
            "APPEND" => Vec::<String>::from([
                String::from("name"),
                String::from("append"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("write,denyoom,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "SADD" => Vec::<String>::from([
                String::from("name"),
                String::from("sadd"),
                String::from("arity"),
                String::from("-3"),
                String::from("flags"),
                String::from("write,denyoom,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "SREM" => Vec::<String>::from([
                String::from("name"),
                String::from("srem"),
                String::from("arity"),
                String::from("-3"),
                String::from("flags"),
                String::from("write,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "SMEMBERS" => Vec::<String>::from([
                String::from("name"),
                String::from("smembers"),
                String::from("arity"),
                String::from("2"),
                String::from("flags"),
                String::from("readonly,sort_for_script"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "SISMEMBER" => Vec::<String>::from([
                String::from("name"),
                String::from("sismember"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("readonly,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "SCARD" => Vec::<String>::from([
                String::from("name"),
                String::from("scard"),
                String::from("arity"),
                String::from("2"),
                String::from("flags"),
                String::from("readonly,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "SPOP" => Vec::<String>::from([
                String::from("name"),
                String::from("spop"),
                String::from("arity"),
                String::from("-2"),
                String::from("flags"),
                String::from("write,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "SRANDMEMBER" => Vec::<String>::from([
                String::from("name"),
                String::from("srandmember"),
                String::from("arity"),
                String::from("-2"),
                String::from("flags"),
                String::from("readonly,random"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "EXISTS" => Vec::<String>::from([
                String::from("name"),
                String::from("exists"),
                String::from("arity"),
                String::from("-2"),
                String::from("flags"),
                String::from("readonly,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("-1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "DEL" => Vec::<String>::from([
                String::from("name"),
                String::from("del"),
                String::from("arity"),
                String::from("-2"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("-1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "UNLINK" => Vec::<String>::from([
                String::from("name"),
                String::from("unlink"),
                String::from("arity"),
                String::from("-2"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("-1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "TOUCH" => Vec::<String>::from([
                String::from("name"),
                String::from("touch"),
                String::from("arity"),
                String::from("-2"),
                String::from("flags"),
                String::from("readonly,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("-1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "RANDOMKEY" => Vec::<String>::from([
                String::from("name"),
                String::from("randomkey"),
                String::from("arity"),
                String::from("1"),
                String::from("flags"),
                String::from("readonly"),
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("0"),
            ]),
            "RENAME" => Vec::<String>::from([
                String::from("name"),
                String::from("rename"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("2"),
                String::from("steps"),
                String::from("1"),
            ]),
            "RENAMENX" => Vec::<String>::from([
                String::from("name"),
                String::from("renamenx"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("2"),
                String::from("steps"),
                String::from("1"),
            ]),
            "COPY" => Vec::<String>::from([
                String::from("name"),
                String::from("copy"),
                String::from("arity"),
                String::from("-3"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("2"),
                String::from("steps"),
                String::from("1"),
            ]),
            "MOVE" => Vec::<String>::from([
                String::from("name"),
                String::from("move"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "EXPIRE" => Vec::<String>::from([
                String::from("name"),
                String::from("expire"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "PEXPIRE" => Vec::<String>::from([
                String::from("name"),
                String::from("pexpire"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "EXPIREAT" => Vec::<String>::from([
                String::from("name"),
                String::from("expireat"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "PEXPIREAT" => Vec::<String>::from([
                String::from("name"),
                String::from("pexpireat"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "TTL" => Vec::<String>::from([
                String::from("name"),
                String::from("ttl"),
                String::from("arity"),
                String::from("2"),
                String::from("flags"),
                String::from("readonly,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "PTTL" => Vec::<String>::from([
                String::from("name"),
                String::from("pttl"),
                String::from("arity"),
                String::from("2"),
                String::from("flags"),
                String::from("readonly,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "EXPIRETIME" => Vec::<String>::from([
                String::from("name"),
                String::from("expiretime"),
                String::from("arity"),
                String::from("2"),
                String::from("flags"),
                String::from("readonly,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "PEXPIRETIME" => Vec::<String>::from([
                String::from("name"),
                String::from("pexpiretime"),
                String::from("arity"),
                String::from("2"),
                String::from("flags"),
                String::from("readonly,fast"),
                String::from("firstkey"),
                String::from("1"),
                String::from("lastkey"),
                String::from("1"),
                String::from("steps"),
                String::from("1"),
            ]),
            "DBSIZE" => Vec::<String>::from([
                String::from("name"),
                String::from("dbsize"),
                String::from("arity"),
                String::from("1"),
                String::from("flags"),
                String::from("readonly"),
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("0"),
            ]),
            "OBJECT" => Vec::<String>::from([
                String::from("name"),
                String::from("object"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("readonly"),
                String::from("firstkey"),
                String::from("2"),
                String::from("lastkey"),
                String::from("2"),
                String::from("steps"),
                String::from("1"),
            ]),
            "FLUSHDB" => Vec::<String>::from([
                String::from("name"),
                String::from("flushdb"),
                String::from("arity"),
                String::from("1"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("0"),
            ]),
            "FLUSHALL" => Vec::<String>::from([
                String::from("name"),
                String::from("flushall"),
                String::from("arity"),
                String::from("1"),
                String::from("flags"),
                String::from("write"),
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("0"),
            ]),
            "SELECT" => Vec::<String>::from([
                String::from("name"),
                String::from("select"),
                String::from("arity"),
                String::from("2"),
                String::from("flags"),
                String::from("fast"),
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("0"),
            ]),
            "SCAN" => Vec::<String>::from([
                String::from("name"),
                String::from("scan"),
                String::from("arity"),
                String::from("-2"),
                String::from("flags"),
                String::from("readonly"),
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("0"),
            ]),
            "WAIT" => Vec::<String>::from([
                String::from("name"),
                String::from("wait"),
                String::from("arity"),
                String::from("3"),
                String::from("flags"),
                String::from("noscript"),
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("0"),
            ]),
            "COMMAND" => Vec::<String>::from([
                String::from("name"),
                String::from("command"),
                String::from("arity"),
                String::from("-1"),
                String::from("flags"),
                String::from("readonly,admin,noscript,loading,stale"), // Example flags
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("1"),
            ]),
            "INFO" => Vec::<String>::from([
                String::from("name"),
                String::from("info"),
                String::from("arity"),
                String::from("-1"),
                String::from("flags"),
                String::from("readonly,loading,stale"), // Example flags
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("1"),
            ]),
            _ => Vec::<String>::from([
                // Default info for unknown commands
                String::from("name"),
                command_name.to_lowercase(),
                String::from("arity"),
                String::from("0"),
                String::from("flags"),
                String::from("unknown"),
                String::from("firstkey"),
                String::from("0"),
                String::from("lastkey"),
                String::from("0"),
                String::from("steps"),
                String::from("1"),
            ]),
        }
    }

    fn get_command_help_resp(command_name: &str) -> Vec<String> {
        // Return RESP array for COMMAND HELP <command_name> (help text)
        // Format compatible with Redis clients
        match command_name.to_uppercase().as_str() {
            "PING" => Vec::<String>::from([
                String::from("PING [message]"),
                String::from("Summary: Ping the server"),
                String::from("Since: 1.0.0"),
                String::from("Group: connection")
            ]),
            "SET" => Vec::<String>::from([
                String::from("SET key value [EX seconds|PX milliseconds] [NX|XX]"),
                String::from("Summary: Set the string value of a key"),
                String::from("Since: 1.0.0"),
                String::from("Group: string")
            ]),
            "MSET" => Vec::<String>::from([
                String::from("MSET key value [key value ...]"),
                String::from("Summary: Set multiple keys to multiple values"),
                String::from("Since: 1.0.0"),
                String::from("Group: string")
            ]),
            "SETNX" => Vec::<String>::from([
                String::from("SETNX key value"),
                String::from("Summary: Set the value of a key only if it does not exist"),
                String::from("Since: 1.0.0"),
                String::from("Group: string")
            ]),
            "GET" => Vec::<String>::from([
                String::from("GET key"),
                String::from("Summary: Get the value of a key"),
                String::from("Since: 1.0.0"),
                String::from("Group: string")
            ]),
            "GETDEL" => Vec::<String>::from([
                String::from("GETDEL key"),
                String::from("Summary: Get the value of a key and delete the key"),
                String::from("Since: 1.0.0"),
                String::from("Group: string")
            ]),
            "GETEX" => Vec::<String>::from([
                String::from("GETEX key [EX seconds|PX milliseconds|EXAT unix-time-seconds|PXAT unix-time-milliseconds|PERSIST]"),
                String::from("Summary: Get the value of a key and optionally set its expiration"),
                String::from("Since: 1.0.0"),
                String::from("Group: string")
            ]),
            "MGET" => Vec::<String>::from([
                String::from("MGET key [key ...]"),
                String::from("Summary: Get the values of all the given keys"),
                String::from("Since: 1.0.0"),
                String::from("Group: string")
            ]),
            "APPEND" => Vec::<String>::from([
                String::from("APPEND key value"),
                String::from("Summary: Append a value to a key"),
                String::from("Since: 1.0.0"),
                String::from("Group: string")
            ]),
            "GETSET" => Vec::<String>::from([
                String::from("GETSET key value"),
                String::from("Summary: Set the string value of a key and return its old value"),
                String::from("Since: 1.0.0"),
                String::from("Group: string")
            ]),
            "SADD" => Vec::<String>::from([
                String::from("SADD key member [member ...]"),
                String::from("Summary: Add one or more members to a set"),
                String::from("Since: 1.0.0"),
                String::from("Group: set")
            ]),
            "SREM" => Vec::<String>::from([
                String::from("SREM key member [member ...]"),
                String::from("Summary: Remove one or more members from a set"),
                String::from("Since: 1.0.0"),
                String::from("Group: set")
            ]),
            "SMEMBERS" => Vec::<String>::from([
                String::from("SMEMBERS key"),
                String::from("Summary: Get all the members in a set"),
                String::from("Since: 1.0.0"),
                String::from("Group: set")
            ]),
            "SISMEMBER" => Vec::<String>::from([
                String::from("SISMEMBER key member"),
                String::from("Summary: Determine if a given value is a member of a set"),
                String::from("Since: 1.0.0"),
                String::from("Group: set")
            ]),
            "SCARD" => Vec::<String>::from([
                String::from("SCARD key"),
                String::from("Summary: Get the number of members in a set"),
                String::from("Since: 1.0.0"),
                String::from("Group: set")
            ]),
            "SPOP" => Vec::<String>::from([
                String::from("SPOP key [count]"),
                String::from("Summary: Remove and return one or multiple random members from a set"),
                String::from("Since: 1.0.0"),
                String::from("Group: set")
            ]),
            "SRANDMEMBER" => Vec::<String>::from([
                String::from("SRANDMEMBER key [count]"),
                String::from("Summary: Get one or multiple random members from a set"),
                String::from("Since: 1.0.0"),
                String::from("Group: set")
            ]),
            "MSETNX" => Vec::<String>::from([
                String::from("MSETNX key value [key value ...]"),
                String::from("Summary: Set multiple keys to multiple values, only if none of the keys exist"),
                String::from("Since: 1.0.0"),
                String::from("Group: string")
            ]),
            "RANDOMKEY" => Vec::<String>::from([
                String::from("RANDOMKEY"),
                String::from("Summary: Return a random key from the keyspace"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "TOUCH" => Vec::<String>::from([
                String::from("TOUCH key [key ...]"),
                String::from("Summary: Alter the last access time of one or more keys"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "UNLINK" => Vec::<String>::from([
                String::from("UNLINK key [key ...]"),
                String::from("Summary: Delete one or more keys (synchronous in current CALOD)"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "RENAME" => Vec::<String>::from([
                String::from("RENAME key newkey"),
                String::from("Summary: Rename a key"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "RENAMENX" => Vec::<String>::from([
                String::from("RENAMENX key newkey"),
                String::from("Summary: Rename a key only if the new key does not exist"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "COPY" => Vec::<String>::from([
                String::from("COPY source destination [DB destination-db] [REPLACE]"),
                String::from("Summary: Copy the value stored at source key to destination key"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "MOVE" => Vec::<String>::from([
                String::from("MOVE key db"),
                String::from("Summary: Move key to another database (db0-only compatibility in CALOD)"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "EXPIREAT" => Vec::<String>::from([
                String::from("EXPIREAT key unix-time-seconds"),
                String::from("Summary: Set expiration for a key as a UNIX timestamp in seconds"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "PEXPIREAT" => Vec::<String>::from([
                String::from("PEXPIREAT key unix-time-milliseconds"),
                String::from("Summary: Set expiration for a key as a UNIX timestamp in milliseconds"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "EXPIRETIME" => Vec::<String>::from([
                String::from("EXPIRETIME key"),
                String::from("Summary: Get the expiration UNIX timestamp for a key in seconds"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "PEXPIRETIME" => Vec::<String>::from([
                String::from("PEXPIRETIME key"),
                String::from("Summary: Get the expiration UNIX timestamp for a key in milliseconds"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "OBJECT" => Vec::<String>::from([
                String::from("OBJECT <subcommand> key"),
                String::from("Summary: Inspect internals of a key"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "WAIT" => Vec::<String>::from([
                String::from("WAIT numreplicas timeout"),
                String::from("Summary: Wait for synchronous replication (single-node compatibility in CALOD)"),
                String::from("Since: 1.0.0"),
                String::from("Group: generic")
            ]),
            "COMMAND" => Vec::<String>::from([
                String::from("COMMAND [subcommand [arg [arg ...]]]"),
                String::from("Summary: Get array of Redis command details"),
                String::from("Since: 2.8.13"),
                String::from("Group: server")
            ]),
            "INFO" => Vec::<String>::from([
                String::from("INFO [section]"),
                String::from("Summary: Get information and statistics about the server"),
                String::from("Since: 1.0.0"),
                String::from("Group: server")
            ]),
            _ => Vec::<String>::from([
                format!("{} (no additional help available)", command_name.to_uppercase()),
                String::from("Summary: Command help not available"),
                String::from("Group: unknown")
            ])
        }
    }
}

fn encode_bulk(arg: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + arg.len().to_string().len() + 2 + arg.len() + 2);
    out.push(b'$');
    out.extend_from_slice(arg.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(arg.as_bytes());
    out.extend_from_slice(b"\r\n");
    out
}

fn encode_array(args: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + args.len() * 24);
    out.push(b'*');
    out.extend_from_slice(args.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    for arg in args {
        out.extend_from_slice(&encode_bulk(arg));
    }
    out
}

fn command_to_resp_frame(cmd: &Command) -> Option<Vec<u8>> {
    let arr: Vec<String> = match cmd {
        Command::Set {
            key,
            value,
            expire_option,
            set_option,
        } => {
            let mut a = vec![
                "SET".to_string(),
                key.clone(),
                String::from_utf8_lossy(value).into_owned(),
            ];
            if let Some(exp) = expire_option {
                match exp {
                    super::command::SetExpireOption::EX(s) => {
                        a.push("EX".to_string());
                        a.push(s.to_string());
                    }
                    super::command::SetExpireOption::PX(ms) => {
                        a.push("PX".to_string());
                        a.push(ms.to_string());
                    }
                }
            }
            if let Some(opt) = set_option {
                match opt {
                    super::command::SetOption::NX => a.push("NX".to_string()),
                    super::command::SetOption::XX => a.push("XX".to_string()),
                }
            }
            a
        }
        Command::SetNx { key, value } => {
            vec![
                "SETNX".to_string(),
                key.clone(),
                String::from_utf8_lossy(value).into_owned(),
            ]
        }
        Command::Quit {} => vec!["QUIT".to_string()],
        Command::Hello {
            protover,
            auth,
            setname,
        } => {
            let mut a = vec!["HELLO".to_string()];
            if let Some(v) = protover {
                a.push(v.to_string());
            }
            if let Some((user, pass)) = auth {
                a.push("AUTH".to_string());
                a.push(user.clone());
                a.push(pass.clone());
            }
            if let Some(name) = setname {
                a.push("SETNAME".to_string());
                a.push(name.clone());
            }
            a
        }
        Command::Select { db } => vec!["SELECT".to_string(), db.to_string()],
        Command::FlushDb {} => vec!["FLUSHDB".to_string()],
        Command::FlushAll {} => vec!["FLUSHALL".to_string()],
        Command::Pttl { key } => vec!["PTTL".to_string(), key.clone()],
        Command::Ttl { key } => vec!["TTL".to_string(), key.clone()],
        Command::DbSize {} => vec!["DBSIZE".to_string()],
        Command::Scan {
            cursor,
            pattern,
            count,
        } => {
            let mut a = vec!["SCAN".to_string(), cursor.to_string()];
            if let Some(p) = pattern {
                a.push("MATCH".to_string());
                a.push(p.clone());
            }
            if let Some(c) = count {
                a.push("COUNT".to_string());
                a.push(c.to_string());
            }
            a
        }
        Command::SetEx {
            key,
            seconds,
            value,
        } => vec![
            "SETEX".to_string(),
            key.clone(),
            seconds.to_string(),
            String::from_utf8_lossy(value).into_owned(),
        ],
        Command::PSetEx {
            key,
            milliseconds,
            value,
        } => vec![
            "PSETEX".to_string(),
            key.clone(),
            milliseconds.to_string(),
            String::from_utf8_lossy(value).into_owned(),
        ],
        Command::Del { keys } => {
            let mut a = vec!["DEL".to_string()];
            a.extend(keys.iter().cloned());
            a
        }
        Command::Unlink { keys } => {
            let mut a = vec!["UNLINK".to_string()];
            a.extend(keys.iter().cloned());
            a
        }
        Command::Touch { keys } => {
            let mut a = vec!["TOUCH".to_string()];
            a.extend(keys.iter().cloned());
            a
        }
        Command::RandomKey {} => vec!["RANDOMKEY".to_string()],
        Command::Rename { key, new_key } => {
            vec!["RENAME".to_string(), key.clone(), new_key.clone()]
        }
        Command::RenameNx { key, new_key } => {
            vec!["RENAMENX".to_string(), key.clone(), new_key.clone()]
        }
        Command::Copy {
            source,
            destination,
            destination_db,
            replace,
        } => {
            let mut a = vec!["COPY".to_string(), source.clone(), destination.clone()];
            if let Some(db) = destination_db {
                a.push("DB".to_string());
                a.push(db.to_string());
            }
            if *replace {
                a.push("REPLACE".to_string());
            }
            a
        }
        Command::Move { key, db } => vec!["MOVE".to_string(), key.clone(), db.to_string()],
        Command::Expire { key, seconds } => {
            vec!["EXPIRE".to_string(), key.clone(), seconds.to_string()]
        }
        Command::PExpire { key, milliseconds } => {
            vec!["PEXPIRE".to_string(), key.clone(), milliseconds.to_string()]
        }
        Command::ExpireAt { key, timestamp } => {
            vec!["EXPIREAT".to_string(), key.clone(), timestamp.to_string()]
        }
        Command::PExpireAt {
            key,
            milliseconds_timestamp,
        } => vec![
            "PEXPIREAT".to_string(),
            key.clone(),
            milliseconds_timestamp.to_string(),
        ],
        Command::ExpireTime { key } => vec!["EXPIRETIME".to_string(), key.clone()],
        Command::PExpireTime { key } => vec!["PEXPIRETIME".to_string(), key.clone()],
        Command::Persist { key } => vec!["PERSIST".to_string(), key.clone()],
        Command::GetDel { key } => vec!["GETDEL".to_string(), key.clone()],
        Command::GetEx { key, option } => {
            let mut a = vec!["GETEX".to_string(), key.clone()];
            if let Some(opt) = option {
                match opt {
                    super::command::GetExOption::EX(v) => {
                        a.push("EX".to_string());
                        a.push(v.to_string());
                    }
                    super::command::GetExOption::PX(v) => {
                        a.push("PX".to_string());
                        a.push(v.to_string());
                    }
                    super::command::GetExOption::EXAT(v) => {
                        a.push("EXAT".to_string());
                        a.push(v.to_string());
                    }
                    super::command::GetExOption::PXAT(v) => {
                        a.push("PXAT".to_string());
                        a.push(v.to_string());
                    }
                    super::command::GetExOption::Persist => {
                        a.push("PERSIST".to_string());
                    }
                }
            }
            a
        }
        Command::ObjectEncoding { key } => {
            vec!["OBJECT".to_string(), "ENCODING".to_string(), key.clone()]
        }
        Command::ObjectRefCount { key } => {
            vec!["OBJECT".to_string(), "REFCOUNT".to_string(), key.clone()]
        }
        Command::ObjectIdleTime { key } => {
            vec!["OBJECT".to_string(), "IDLETIME".to_string(), key.clone()]
        }
        Command::Wait {
            numreplicas,
            timeout_ms,
        } => vec![
            "WAIT".to_string(),
            numreplicas.to_string(),
            timeout_ms.to_string(),
        ],
        Command::Append { key, value } => vec![
            "APPEND".to_string(),
            key.clone(),
            String::from_utf8_lossy(value).into_owned(),
        ],
        Command::SetRange { key, offset, value } => vec![
            "SETRANGE".to_string(),
            key.clone(),
            offset.to_string(),
            String::from_utf8_lossy(value).into_owned(),
        ],
        Command::GetSet { key, value } => vec![
            "GETSET".to_string(),
            key.clone(),
            String::from_utf8_lossy(value).into_owned(),
        ],
        Command::MSet { key_values } => {
            let mut a = vec!["MSET".to_string()];
            for (k, v) in key_values {
                a.push(k.clone());
                a.push(String::from_utf8_lossy(v).into_owned());
            }
            a
        }
        Command::MSetNx { key_values } => {
            let mut a = vec!["MSETNX".to_string()];
            for (k, v) in key_values {
                a.push(k.clone());
                a.push(String::from_utf8_lossy(v).into_owned());
            }
            a
        }
        Command::SAdd { key, members } => {
            let mut a = vec!["SADD".to_string(), key.clone()];
            a.extend(
                members
                    .iter()
                    .map(|m| String::from_utf8_lossy(m).into_owned()),
            );
            a
        }
        Command::SRem { key, members } => {
            let mut a = vec!["SREM".to_string(), key.clone()];
            a.extend(
                members
                    .iter()
                    .map(|m| String::from_utf8_lossy(m).into_owned()),
            );
            a
        }
        Command::SMembers { key } => vec!["SMEMBERS".to_string(), key.clone()],
        Command::SIsMember { key, member } => {
            vec![
                "SISMEMBER".to_string(),
                key.clone(),
                String::from_utf8_lossy(member).into_owned(),
            ]
        }
        Command::SCard { key } => vec!["SCARD".to_string(), key.clone()],
        Command::SPop { key, count } => {
            let mut a = vec!["SPOP".to_string(), key.clone()];
            if let Some(c) = count {
                a.push(c.to_string());
            }
            a
        }
        Command::SRandMember { key, count } => {
            let mut a = vec!["SRANDMEMBER".to_string(), key.clone()];
            if let Some(c) = count {
                a.push(c.to_string());
            }
            a
        }
        Command::Incr { key } => vec!["INCR".to_string(), key.clone()],
        Command::Decr { key } => vec!["DECR".to_string(), key.clone()],
        Command::IncrBy { key, increment } => {
            vec!["INCRBY".to_string(), key.clone(), increment.to_string()]
        }
        Command::DecrBy { key, decrement } => {
            vec!["DECRBY".to_string(), key.clone(), decrement.to_string()]
        }
        Command::IncrByFloat { key, increment } => vec![
            "INCRBYFLOAT".to_string(),
            key.clone(),
            increment.to_string(),
        ],
        Command::HSet { key, field_values } => {
            let mut a = vec!["HSET".to_string(), key.clone()];
            for (f, v) in field_values {
                a.push(String::from_utf8_lossy(f).into_owned());
                a.push(String::from_utf8_lossy(v).into_owned());
            }
            a
        }
        Command::HDel { key, fields } => {
            let mut a = vec!["HDEL".to_string(), key.clone()];
            a.extend(
                fields
                    .iter()
                    .map(|f| String::from_utf8_lossy(f).into_owned()),
            );
            a
        }
        Command::HIncrBy {
            key,
            field,
            increment,
        } => {
            vec![
                "HINCRBY".to_string(),
                key.clone(),
                String::from_utf8_lossy(field).into_owned(),
                increment.to_string(),
            ]
        }
        Command::HIncrByFloat {
            key,
            key_field_increment,
        } => {
            let mut a = vec!["HINCRBYFLOAT".to_string(), key.clone()];
            for (f, inc) in key_field_increment {
                a.push(String::from_utf8_lossy(f).into_owned());
                a.push(inc.to_string());
            }
            a
        }
        Command::HMSet { key, field_values } => {
            let mut a = vec!["HMSET".to_string(), key.clone()];
            for (f, v) in field_values {
                a.push(String::from_utf8_lossy(f).into_owned());
                a.push(String::from_utf8_lossy(v).into_owned());
            }
            a
        }
        Command::HSetNx { key, field, value } => {
            vec![
                "HSETNX".to_string(),
                key.clone(),
                String::from_utf8_lossy(field).into_owned(),
                String::from_utf8_lossy(value).into_owned(),
            ]
        }
        Command::ListLPush { key, values } => {
            let mut a = vec!["LPUSH".to_string(), key.clone()];
            a.extend(
                values
                    .iter()
                    .map(|v| String::from_utf8_lossy(v).into_owned()),
            );
            a
        }
        Command::ListRPush { key, values } => {
            let mut a = vec!["RPUSH".to_string(), key.clone()];
            a.extend(
                values
                    .iter()
                    .map(|v| String::from_utf8_lossy(v).into_owned()),
            );
            a
        }
        Command::LPop { key } => vec!["LPOP".to_string(), key.clone()],
        Command::RPop { key } => vec!["RPOP".to_string(), key.clone()],
        Command::LInsert {
            key,
            before_after,
            pivot,
            value,
        } => vec![
            "LINSERT".to_string(),
            key.clone(),
            match before_after {
                super::command::InsertOption::Before => "BEFORE".to_string(),
                super::command::InsertOption::After => "AFTER".to_string(),
            },
            String::from_utf8_lossy(pivot).into_owned(),
            String::from_utf8_lossy(value).into_owned(),
        ],
        Command::LSet { key, index, value } => {
            vec![
                "LSET".to_string(),
                key.clone(),
                index.to_string(),
                String::from_utf8_lossy(value).into_owned(),
            ]
        }
        Command::LTrim { key, start, end } => {
            vec![
                "LTRIM".to_string(),
                key.clone(),
                start.to_string(),
                end.to_string(),
            ]
        }
        Command::LRem { key, count, value } => {
            vec![
                "LREM".to_string(),
                key.clone(),
                count.to_string(),
                String::from_utf8_lossy(value).into_owned(),
            ]
        }
        Command::RPopLPush {
            source,
            destination,
        } => {
            vec!["RPOPLPUSH".to_string(), source.clone(), destination.clone()]
        }
        Command::JsonSet { key, path, value } => vec![
            "JSON.SET".to_string(),
            key.clone(),
            path.clone(),
            value.clone(),
        ],
        Command::JsonDel { key, path } => vec!["JSON.DEL".to_string(), key.clone(), path.clone()],
        Command::JsonNumIncrBy {
            key,
            path,
            increment,
        } => {
            vec![
                "JSON.NUMINCRBY".to_string(),
                key.clone(),
                path.clone(),
                increment.to_string(),
            ]
        }
        Command::JsonStrAppend { key, path, value } => {
            vec![
                "JSON.STRAPPEND".to_string(),
                key.clone(),
                path.clone(),
                value.clone(),
            ]
        }
        Command::JsonArrAppend { key, path, values } => {
            let mut a = vec!["JSON.ARRAPPEND".to_string(), key.clone(), path.clone()];
            a.extend(values.iter().cloned());
            a
        }
        Command::JsonObjSet {
            key,
            path,
            key_to_set,
            value,
        } => {
            vec![
                "JSON.OBJSET".to_string(),
                key.clone(),
                path.clone(),
                key_to_set.clone(),
                value.clone(),
            ]
        }
        Command::JsonArrInsert {
            key,
            path,
            index,
            values,
        } => {
            let mut a = vec![
                "JSON.ARRINSERT".to_string(),
                key.clone(),
                path.clone(),
                index.to_string(),
            ];
            a.extend(values.iter().cloned());
            a
        }
        Command::JsonArrPop { key, path, index } => {
            let mut a = vec!["JSON.ARRPOP".to_string(), key.clone(), path.clone()];
            if let Some(idx) = index {
                a.push(idx.to_string());
            }
            a
        }
        Command::JsonArrTrim {
            key,
            path,
            start,
            stop,
        } => {
            vec![
                "JSON.ARRTRIM".to_string(),
                key.clone(),
                path.clone(),
                start.to_string(),
                stop.to_string(),
            ]
        }
        Command::GraphCreateNode {
            key,
            node_id,
            properties,
        } => {
            let mut a = vec![
                "GRAPH.CREATE_NODE".to_string(),
                key.clone(),
                node_id.clone(),
            ];
            for (k, v) in properties {
                a.push(k.clone());
                a.push(v.clone());
            }
            a
        }
        Command::GraphDeleteNode { key, node_id } => {
            vec![
                "GRAPH.DELETE_NODE".to_string(),
                key.clone(),
                node_id.clone(),
            ]
        }
        Command::GraphCreateEdge {
            key,
            edge_id,
            source_node_id,
            target_node_id,
            relation_type,
            properties,
        } => {
            let mut a = vec![
                "GRAPH.CREATE_EDGE".to_string(),
                key.clone(),
                edge_id.clone(),
                source_node_id.clone(),
                target_node_id.clone(),
                relation_type.clone(),
            ];
            for (k, v) in properties {
                a.push(k.clone());
                a.push(v.clone());
            }
            a
        }
        Command::GraphDeleteEdge { key, edge_id } => {
            vec![
                "GRAPH.DELETE_EDGE".to_string(),
                key.clone(),
                edge_id.clone(),
            ]
        }
        Command::GraphSetNodeProperty {
            key,
            node_id,
            property_key,
            property_value,
        } => vec![
            "GRAPH.SET_NODE_PROPERTY".to_string(),
            key.clone(),
            node_id.clone(),
            property_key.clone(),
            property_value.clone(),
        ],
        Command::GraphDeleteNodeProperty {
            key,
            node_id,
            property_key,
        } => vec![
            "GRAPH.DELETE_NODE_PROPERTY".to_string(),
            key.clone(),
            node_id.clone(),
            property_key.clone(),
        ],
        Command::GraphSetEdgeProperty {
            key,
            edge_id,
            property_key,
            property_value,
        } => vec![
            "GRAPH.SET_EDGE_PROPERTY".to_string(),
            key.clone(),
            edge_id.clone(),
            property_key.clone(),
            property_value.clone(),
        ],
        Command::GraphDeleteEdgeProperty {
            key,
            edge_id,
            property_key,
        } => vec![
            "GRAPH.DELETE_EDGE_PROPERTY".to_string(),
            key.clone(),
            edge_id.clone(),
            property_key.clone(),
        ],
        _ => return None,
    };

    Some(encode_array(&arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_missing_key_as_client_error() {
        assert_eq!(
            ServedCalodStore::command_error_log_level(&CacheError::KeyNotFound(
                "missing".to_string()
            )),
            CommandErrorLogLevel::Debug
        );
    }

    #[test]
    fn classifies_internal_error_as_server_error() {
        assert_eq!(
            ServedCalodStore::command_error_log_level(&CacheError::InternalError(
                "boom".to_string()
            )),
            CommandErrorLogLevel::Error
        );
    }
}
