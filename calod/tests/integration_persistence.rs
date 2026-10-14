use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use calod::cld_srv::auth::AuthManager;
use calod::cld_srv::command::Command;
use calod::cld_srv::connection::{Connection, ConnectionConfig};
use calod::cld_srv::metrics::MetricsAggregator;
use calod::cld_srv::protocol::ConnCtx;
use calod::cld_srv::served_store::ServedCalodStore;
use calod::store::calod_data::DataType;
use calod::store::config::CalodConfig;
use calod::store::persistence::CachePersistence;
use calod::store::sharded_store::ShardedStore;
use dashmap::DashMap;
use tokio::sync::Mutex;

fn temp_dir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("calod-test-{}", uuid::Uuid::new_v4()));
    p
}

fn wal_config(dir: &std::path::Path) -> CalodConfig {
    let mut cfg = CalodConfig::default();
    cfg.persistence.enable_snapshot = false;
    cfg.persistence.enable_aof = true;
    cfg.persistence.aof_dir = dir.to_path_buf();
    cfg.persistence.snapshot_dir = dir.to_path_buf();
    cfg
}

struct Ctx {
    store: Arc<ServedCalodStore>,
    active: Arc<DashMap<uuid::Uuid, std::sync::Weak<Mutex<Connection>>, ahash::RandomState>>,
    conn: Arc<Mutex<Connection>>,
}

impl Ctx {
    async fn new(cfg: CalodConfig) -> Self {
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

    async fn exec_raw(&self, cmd: Command) -> Vec<u8> {
        self.store
            .execute(
                cmd,
                self.active.clone(),
                self.conn.clone(),
                ConnCtx::default(),
            )
            .await
    }
}

#[tokio::test]
async fn snapshot_save_and_load_roundtrip() {
    let cfg = CalodConfig::default();
    let store = ShardedStore::new(cfg.clone());

    let _ = store
        .set_cmd("k1".to_string(), b"v1".to_vec(), None, None)
        .await;
    let _ = store
        .set_cmd("k2".to_string(), b"v2".to_vec(), None, None)
        .await;

    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");

    store
        .save(dir.to_str().expect("utf8 path"))
        .await
        .expect("save should succeed");

    let reloaded = ShardedStore::new(cfg);
    reloaded
        .load(dir.to_str().expect("utf8 path"))
        .await
        .expect("load should succeed");

    let v1 = reloaded.get_cmd("k1").await.expect("k1");
    let v2 = reloaded.get_cmd("k2").await.expect("k2");

    match v1 {
        DataType::String(s) => assert_eq!(s, b"v1"),
        other => panic!("unexpected type for k1: {other:?}"),
    }

    match v2 {
        DataType::String(s) => assert_eq!(s, b"v2"),
        other => panic!("unexpected type for k2: {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn wal_replay_recovers_mutations_after_restart() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");
    let cfg = wal_config(&dir);

    let ctx = Ctx::new(cfg.clone()).await;

    let _ = ctx
        .exec_raw(Command::Set {
            key: "wal:k1".to_string(),
            value: b"v1".to_vec(),
            expire_option: None,
            set_option: None,
        })
        .await;
    let _ = ctx
        .exec_raw(Command::SAdd {
            key: "wal:set".to_string(),
            members: vec![b"a".to_vec(), b"b".to_vec()],
        })
        .await;
    let _ = ctx
        .exec_raw(Command::GetDel {
            key: "wal:k1".to_string(),
        })
        .await;
    ctx.store.flush_wal().await.expect("flush wal");
    drop(ctx);

    let recovered = ServedCalodStore::try_new_with_config(cfg)
        .await
        .expect("recover store");
    let inner = recovered.inner_store();

    match inner.get_cmd("wal:k1").await {
        Ok(DataType::Nil) => {}
        Err(calod::store::error::CacheError::KeyNotFound(_)) => {}
        other => panic!("unexpected recovered key state: {other:?}"),
    }

    let set_len = inner
        .scard_cmd("wal:set")
        .await
        .expect("recover set members");
    match set_len {
        DataType::Integer(value) => assert_eq!(value, 2),
        other => panic!("unexpected recovered set length response: {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn snapshot_and_wal_recovery_restore_latest_state() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");

    let mut cfg = CalodConfig::default();
    cfg.persistence.enable_snapshot = true;
    cfg.persistence.enable_aof = true;
    cfg.persistence.snapshot_dir = dir.clone();
    cfg.persistence.aof_dir = dir.clone();

    let ctx = Ctx::new(cfg.clone()).await;

    let _ = ctx
        .exec_raw(Command::Set {
            key: "snap:key".to_string(),
            value: b"v1".to_vec(),
            expire_option: None,
            set_option: None,
        })
        .await;
    let saved = ctx.exec_raw(Command::Save {}).await;
    assert_eq!(saved, b"+OK\r\n", "SAVE must succeed");
    // The verified snapshot layout: per-DB shard dirs plus a manifest.
    assert!(dir.join("db_0").join("shard_0.bin").exists());
    assert!(dir.join("manifest.json").exists());

    let _ = ctx
        .exec_raw(Command::Set {
            key: "snap:key".to_string(),
            value: b"v2".to_vec(),
            expire_option: None,
            set_option: None,
        })
        .await;
    let _ = ctx
        .exec_raw(Command::Set {
            key: "snap:key2".to_string(),
            value: b"v3".to_vec(),
            expire_option: None,
            set_option: None,
        })
        .await;
    ctx.store.flush_wal().await.expect("flush wal");
    drop(ctx);

    let recovered = ServedCalodStore::try_new_with_config(cfg)
        .await
        .expect("recover store");
    let inner = recovered.inner_store();

    match inner.get_cmd("snap:key").await.expect("snap:key") {
        DataType::String(value) => assert_eq!(value, b"v2"),
        other => panic!("unexpected value for snap:key: {other:?}"),
    }

    match inner.get_cmd("snap:key2").await.expect("snap:key2") {
        DataType::String(value) => assert_eq!(value, b"v3"),
        other => panic!("unexpected value for snap:key2: {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn multidb_snapshot_and_wal_recovery_restores_each_db() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");

    let mut cfg = CalodConfig::default();
    cfg.persistence.enable_snapshot = true;
    cfg.persistence.enable_aof = true;
    cfg.persistence.snapshot_dir = dir.clone();
    cfg.persistence.aof_dir = dir.clone();

    let ctx = Ctx::new(cfg.clone()).await;
    let exec = |cmd: Command, db: u8| {
        let ctx = &ctx;
        async move {
            ctx.store
                .execute(
                    cmd,
                    ctx.active.clone(),
                    ctx.conn.clone(),
                    ConnCtx {
                        db,
                        ..ConnCtx::default()
                    },
                )
                .await
        }
    };

    assert_eq!(
        exec(
            Command::Set {
                key: "plain".to_string(),
                value: b"hello".to_vec(),
                expire_option: None,
                set_option: None,
            },
            0,
        )
        .await,
        b"+OK\r\n"
    );
    assert_eq!(
        exec(
            Command::Set {
                key: "blob".to_string(),
                value: b"xyz".to_vec(),
                expire_option: None,
                set_option: None,
            },
            5,
        )
        .await,
        b"+OK\r\n"
    );
    assert_eq!(exec(Command::Save {}, 0).await, b"+OK\r\n", "SAVE");
    assert!(dir.join("db_0").join("shard_0.bin").exists());
    assert!(dir.join("db_5").join("shard_0.bin").exists());
    ctx.store.flush_wal().await.expect("flush wal");
    drop(ctx);

    let ctx = Ctx::new(cfg).await;
    let get = |key: &str, db: u8| {
        let ctx = &ctx;
        let key = key.to_string();
        async move {
            ctx.store
                .execute(
                    Command::Get { key },
                    ctx.active.clone(),
                    ctx.conn.clone(),
                    ConnCtx {
                        db,
                        ..ConnCtx::default()
                    },
                )
                .await
        }
    };
    assert_eq!(get("plain", 0).await, b"$5\r\nhello\r\n", "plain from db0");
    assert_eq!(get("blob", 5).await, b"$3\r\nxyz\r\n", "blob from db5");

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn incomplete_wal_tail_is_recovered_and_truncated() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");
    let cfg = wal_config(&dir);

    let ctx = Ctx::new(cfg.clone()).await;
    let _ = ctx
        .exec_raw(Command::Set {
            key: "tail:key".to_string(),
            value: b"ok".to_vec(),
            expire_option: None,
            set_option: None,
        })
        .await;
    ctx.store.flush_wal().await.expect("flush wal");
    drop(ctx);

    // Simulate a crash mid-append: a partial record glued to the segment tail.
    let segment_path = dir.join("wal-00000001.log");
    let mut bytes = fs::read(&segment_path).expect("read wal segment");
    let original_len = bytes.len();
    bytes.extend_from_slice(b"CW\x01\x00\x20\x00");
    fs::write(&segment_path, &bytes).expect("append truncated tail");

    let recovered = ServedCalodStore::try_new_with_config(cfg)
        .await
        .expect("recover store");
    let inner = recovered.inner_store();

    match inner.get_cmd("tail:key").await.expect("tail:key") {
        DataType::String(value) => assert_eq!(value, b"ok"),
        other => panic!("unexpected value after truncated-tail recovery: {other:?}"),
    }

    let repaired_len = fs::read(&segment_path)
        .expect("read repaired segment")
        .len();
    assert_eq!(repaired_len, original_len);

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn corrupt_wal_segment_fails_recovery() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");
    let cfg = wal_config(&dir);

    // A full-length record with bad magic is real corruption, not a crash
    // tail: recovery must fail closed.
    fs::write(
        dir.join("wal-00000001.log"),
        b"not-a-valid-wal-record-at-all",
    )
    .expect("write corrupt segment");

    match ServedCalodStore::try_new_with_config(cfg).await {
        Ok(_) => panic!("corrupt WAL segment should fail recovery"),
        Err(err) => {
            let err_text = err.to_string();
            assert!(
                err_text.contains("corrupt"),
                "expected corruption error, got: {err_text}"
            );
        }
    }

    let _ = fs::remove_dir_all(&dir);
}
