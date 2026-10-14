//! Startup safety coverage (Phase P1a): fail-closed persistence recovery, the
//! `--allow-data-loss` escape hatch, and CONFIG GET reporting the effective
//! (resolved-at-startup) configuration.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use calod::cld_srv::auth::AuthManager;
use calod::cld_srv::command::{Command, ConfigSubCommand};
use calod::cld_srv::connection::{Connection, ConnectionConfig};
use calod::cld_srv::metrics::MetricsAggregator;
use calod::cld_srv::protocol::ConnCtx;
use calod::cld_srv::served_store::ServedCalodStore;
use calod::parser::parser::RESPParser;
use calod::parser::value::Value;
use calod::store::config::CalodConfig;

fn temp_dir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("calod-test-{}", uuid::Uuid::new_v4()));
    p
}

fn persistence_config(dir: &Path) -> CalodConfig {
    let mut cfg = CalodConfig::default();
    cfg.persistence.enable_snapshot = true;
    cfg.persistence.enable_aof = false;
    cfg.persistence.snapshot_dir = dir.to_path_buf();
    cfg.persistence.aof_dir = dir.to_path_buf();
    cfg
}

/// Save a real (manifest + checksums) snapshot containing `key` through the
/// store itself, then corrupt `db_0/shard_0.bin` so the next startup faces a
/// snapshot whose manifest CRC no longer matches.
async fn corrupt_snapshot(dir: &Path, key: &str) {
    let store = Arc::new(
        ServedCalodStore::try_new_with_config(persistence_config(dir))
            .await
            .expect("create seeding store"),
    );
    let active = Arc::new(dashmap::DashMap::with_hasher(ahash::RandomState::new()));
    let metrics = Arc::new(MetricsAggregator::new());
    let auth = Arc::new(AuthManager::new(false));
    let conn = Arc::new(tokio::sync::Mutex::new(Connection::new(
        store.clone(),
        ConnectionConfig::default(),
        metrics,
        auth,
    )));
    store
        .execute(
            Command::Set {
                key: key.to_string(),
                value: b"v1".to_vec(),
                expire_option: None,
                set_option: None,
            },
            active.clone(),
            conn.clone(),
            ConnCtx::default(),
        )
        .await;
    let saved = store
        .execute(Command::Save {}, active, conn, ConnCtx::default())
        .await;
    assert_eq!(saved, b"+OK\r\n", "SAVE must succeed");
    drop(store);
    fs::write(
        dir.join("db_0").join("shard_0.bin"),
        b"not-a-valid-bincode-snapshot",
    )
    .expect("corrupt db_0/shard_0.bin");
}

#[tokio::test]
async fn corrupt_snapshot_refuses_startup() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");
    corrupt_snapshot(&dir, "doomed:key").await;

    let result = ServedCalodStore::try_new_with_config(persistence_config(&dir)).await;
    assert!(
        result.is_err(),
        "corrupt snapshot must fail closed instead of silently starting fresh"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn corrupt_snapshot_with_allow_data_loss_starts_empty() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");
    corrupt_snapshot(&dir, "doomed:key").await;

    let mut cfg = persistence_config(&dir);
    cfg.allow_data_loss = true;

    let store = ServedCalodStore::try_new_with_config(cfg)
        .await
        .expect("allow_data_loss must start despite the corrupt snapshot");

    match store.inner_store().get_cmd("doomed:key").await {
        Ok(calod::store::calod_data::DataType::Nil) => {}
        Err(calod::store::error::CacheError::KeyNotFound(_)) => {}
        other => panic!("store must start empty under allow_data_loss: {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn corrupt_wal_with_allow_data_loss_starts_empty() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");
    fs::write(
        dir.join("wal-00000001.log"),
        b"not-a-valid-wal-record-at-all",
    )
    .expect("write corrupt wal segment");

    let mut cfg = persistence_config(&dir);
    cfg.persistence.enable_snapshot = false;
    cfg.persistence.enable_aof = true;

    // Fail-closed by default.
    assert!(ServedCalodStore::try_new_with_config(cfg.clone())
        .await
        .is_err());

    // Escape hatch starts empty.
    cfg.allow_data_loss = true;
    let store = ServedCalodStore::try_new_with_config(cfg)
        .await
        .expect("allow_data_loss must start despite the corrupt WAL");
    match store.inner_store().get_cmd("bad:key").await {
        Ok(calod::store::calod_data::DataType::Nil) => {}
        Err(calod::store::error::CacheError::KeyNotFound(_)) => {}
        other => panic!("store must start empty under allow_data_loss: {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn config_get_reports_effective_startup_values() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");

    let mut cfg = CalodConfig {
        listen_addr: "127.0.0.1:7777".parse().expect("listen addr"),
        max_connections: 1234,
        max_memory_mb: 64,
        ..CalodConfig::default()
    };
    cfg.security.password = Some("s3cret".to_string());
    cfg.persistence.enable_snapshot = true;
    cfg.persistence.enable_aof = true;
    cfg.persistence.snapshot_dir = dir.clone();
    cfg.persistence.aof_dir = dir.clone();

    let store = Arc::new(
        ServedCalodStore::try_new_with_config(cfg)
            .await
            .expect("create store"),
    );
    let active = Arc::new(dashmap::DashMap::with_hasher(ahash::RandomState::new()));
    let metrics = Arc::new(MetricsAggregator::new());
    let auth = Arc::new(AuthManager::new(false));
    let conn = Arc::new(tokio::sync::Mutex::new(Connection::new(
        store.clone(),
        ConnectionConfig::default(),
        metrics,
        auth,
    )));

    let raw = store
        .execute(
            Command::Config {
                sub: ConfigSubCommand::Get {
                    params: vec!["*".to_string()],
                },
            },
            active,
            conn,
            ConnCtx::default(),
        )
        .await;

    let (value, _) = RESPParser::parse_value(&raw).expect("valid CONFIG GET reply");
    let pairs: Vec<(String, String)> = match value {
        Value::Map(entries) => entries
            .into_iter()
            .filter_map(|(k, v)| match (k, v) {
                (Value::BulkString(k), Value::BulkString(v)) => Some((
                    String::from_utf8(k).expect("utf8 key"),
                    String::from_utf8(v).expect("utf8 value"),
                )),
                _ => None,
            })
            .collect(),
        // RESP2 renders maps as a flat k, v, k, v array.
        Value::Array(flat) => flat
            .chunks(2)
            .filter_map(|pair| match pair {
                [Value::BulkString(k), Value::BulkString(v)] => Some((
                    String::from_utf8(k.clone()).expect("utf8 key"),
                    String::from_utf8(v.clone()).expect("utf8 value"),
                )),
                _ => None,
            })
            .collect(),
        other => panic!("unexpected CONFIG GET reply shape: {other:?}"),
    };
    let get = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("CONFIG GET must report {}", name))
    };

    assert_eq!(get("bind"), "127.0.0.1");
    assert_eq!(get("port"), "7777");
    assert_eq!(get("requirepass"), "s3cret");
    assert_eq!(get("maxclients"), "1234");
    assert_eq!(get("maxmemory"), (64 * 1024 * 1024).to_string());
    assert_eq!(get("appendonly"), "yes");
    assert_eq!(get("appendfsync"), "everysec");
    assert_eq!(get("dir"), dir.to_string_lossy());

    let _ = fs::remove_dir_all(&dir);
}
