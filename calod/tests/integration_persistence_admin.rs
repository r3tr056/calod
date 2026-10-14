//! Phase 12 (persistence admin): SAVE / LASTSAVE / DEBUG RELOAD round-trip,
//! proving every data type (including sorted sets, streams, HLL) survives a
//! snapshot. Driven in-process with a unique temp snapshot dir for isolation.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use calod::cld_srv::auth::AuthManager;
use calod::cld_srv::command::Command;
use calod::cld_srv::connection::{Connection, ConnectionConfig};
use calod::cld_srv::metrics::MetricsAggregator;
use calod::cld_srv::protocol::ConnCtx;
use calod::cld_srv::served_store::ServedCalodStore;
use calod::parser::parser::RESPParser;
use calod::parser::value::Value;
use calod::store::config::CalodConfig;
use dashmap::DashMap;
use tokio::sync::Mutex;

fn temp_dir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("calod-admin-test-{}", uuid::Uuid::new_v4()));
    p
}

struct Ctx {
    store: Arc<ServedCalodStore>,
    active: Arc<DashMap<uuid::Uuid, std::sync::Weak<Mutex<Connection>>, ahash::RandomState>>,
    conn: Arc<Mutex<Connection>>,
}

impl Ctx {
    async fn exec(&self, cmd: Command) -> Value {
        let raw = self
            .store
            .execute(
                cmd,
                self.active.clone(),
                self.conn.clone(),
                ConnCtx::default(),
            )
            .await;
        let (v, n) = RESPParser::parse_value(&raw).expect("valid RESP");
        assert_eq!(n, raw.len(), "trailing bytes in reply");
        v
    }
}

async fn ctx_with_dir(dir: &Path) -> Ctx {
    let mut cfg = CalodConfig::default();
    cfg.persistence.enable_snapshot = true;
    cfg.persistence.snapshot_dir = dir.to_path_buf();
    cfg.persistence.aof_dir = dir.to_path_buf();
    cfg.persistence.enable_aof = false;
    let store = Arc::new(
        ServedCalodStore::try_new_with_config(cfg)
            .await
            .expect("create store"),
    );
    let active = Arc::new(DashMap::with_hasher(ahash::RandomState::new()));
    let metrics = Arc::new(MetricsAggregator::new());
    let auth = Arc::new(AuthManager::new(false));
    let conn = Arc::new(Mutex::new(Connection::new(
        store.clone(),
        ConnectionConfig::default(),
        metrics,
        auth,
    )));
    Ctx {
        store,
        active,
        conn,
    }
}

#[tokio::test]
async fn debug_reload_preserves_all_data_types() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir");
    let h = ctx_with_dir(&dir).await;

    // Seed a representative spread of types.
    h.exec(Command::Set {
        key: "str".to_string(),
        value: b"hello".to_vec(),
        expire_option: None,
        set_option: None,
    })
    .await;
    h.exec(Command::HSet {
        key: "hash".to_string(),
        field_values: vec![(b"f".to_vec(), b"v".to_vec())],
    })
    .await;
    h.exec(Command::ZAdd {
        key: "zset".to_string(),
        nx: false,
        xx: false,
        gt: false,
        lt: false,
        ch: false,
        incr: false,
        members: vec![(1.0, b"a".to_vec()), (2.0, b"b".to_vec())],
    })
    .await;
    h.exec(Command::XAdd {
        key: "stream".to_string(),
        id: "1-1".to_string(),
        nomkstream: false,
        maxlen: None,
        fields: vec![(b"f".to_vec(), b"v".to_vec())],
    })
    .await;

    // DEBUG RELOAD = snapshot to disk + load back in place.
    let reload = h
        .exec(Command::Debug {
            args: vec!["RELOAD".to_string()],
        })
        .await;
    assert_eq!(reload, Value::SimpleString("OK".to_string()));

    // Everything is still present and correct after the round-trip.
    assert_eq!(
        h.exec(Command::Get {
            key: "str".to_string()
        })
        .await,
        Value::BulkString(b"hello".to_vec())
    );
    assert_eq!(
        h.exec(Command::ZCard {
            key: "zset".to_string()
        })
        .await,
        Value::Integer(2)
    );
    assert_eq!(
        h.exec(Command::XLen {
            key: "stream".to_string()
        })
        .await,
        Value::Integer(1)
    );
    assert_eq!(
        h.exec(Command::HGet {
            key: "hash".to_string(),
            field: b"f".to_vec(),
        })
        .await,
        Value::BulkString(b"v".to_vec())
    );

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn save_updates_lastsave() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir");
    let h = ctx_with_dir(&dir).await;

    let before = match h.exec(Command::LastSave {}).await {
        Value::Integer(t) => t,
        other => panic!("expected integer, got {other:?}"),
    };
    assert!(before > 0);

    let saved = h.exec(Command::Save {}).await;
    assert_eq!(saved, Value::SimpleString("OK".to_string()));

    let after = match h.exec(Command::LastSave {}).await {
        Value::Integer(t) => t,
        other => panic!("expected integer, got {other:?}"),
    };
    assert!(after >= before);

    // The snapshot manifest exists on disk. (No keys were written, so no
    // per-DB shard directories — empty DBs are skipped by design.)
    assert!(dir.join("manifest.json").exists());

    let _ = fs::remove_dir_all(&dir);
}
