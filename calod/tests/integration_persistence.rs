use std::fs;
use std::path::PathBuf;

use calod::cld_srv::command::Command;
use calod::cld_srv::served_store::ServedCalodStore;
use calod::store::config::CalodConfig;
use calod::store::persistence::CachePersistence;
use calod::store::sharded_store::ShardedStore;

fn temp_dir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("calod-test-{}", uuid::Uuid::new_v4()));
    p
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
        calod::store::calod_data::DataType::String(s) => assert_eq!(s, b"v1"),
        other => panic!("unexpected type for k1: {other:?}"),
    }

    match v2 {
        calod::store::calod_data::DataType::String(s) => assert_eq!(s, b"v2"),
        other => panic!("unexpected type for k2: {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn aof_replay_recovers_mutations_after_restart() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");

    let mut cfg = CalodConfig::default();
    cfg.persistence.enable_snapshot = false;
    cfg.persistence.enable_aof = true;
    cfg.persistence.aof_dir = dir.clone();
    cfg.persistence.snapshot_dir = dir.clone();

    let store = std::sync::Arc::new(
        ServedCalodStore::try_new_with_config(cfg.clone())
            .await
            .expect("create store"),
    );

    let active = std::sync::Arc::new(dashmap::DashMap::with_hasher(ahash::RandomState::new()));
    let metrics = std::sync::Arc::new(calod::cld_srv::metrics::MetricsAggregator::new());
    let auth = std::sync::Arc::new(calod::cld_srv::auth::AuthManager::new(false));
    let conn = std::sync::Arc::new(tokio::sync::Mutex::new(
        calod::cld_srv::connection::Connection::new(
            store.clone(),
            calod::cld_srv::connection::ConnectionConfig::default(),
            metrics,
            auth,
        ),
    ));

    let _ = store
        .execute(
            Command::Set {
                key: "aof:k1".to_string(),
                value: b"v1".to_vec(),
                expire_option: None,
                set_option: None,
            },
            active.clone(),
            conn.clone(),
            calod::cld_srv::protocol::ConnCtx::default(),
        )
        .await;
    let _ = store
        .execute(
            Command::SAdd {
                key: "aof:set".to_string(),
                members: vec![b"a".to_vec(), b"b".to_vec()],
            },
            active.clone(),
            conn.clone(),
            calod::cld_srv::protocol::ConnCtx::default(),
        )
        .await;
    let _ = store
        .execute(
            Command::GetDel {
                key: "aof:k1".to_string(),
            },
            active,
            conn,
            calod::cld_srv::protocol::ConnCtx::default(),
        )
        .await;
    store.flush_aof().await.expect("flush aof");
    drop(store);

    let recovered = ServedCalodStore::try_new_with_config(cfg)
        .await
        .expect("recover store");
    let inner = recovered.inner_store();

    match inner.get_cmd("aof:k1").await {
        Ok(calod::store::calod_data::DataType::Nil) => {}
        Err(calod::store::error::CacheError::KeyNotFound(_)) => {}
        other => panic!("unexpected recovered key state: {other:?}"),
    }

    let set_len = inner
        .scard_cmd("aof:set")
        .await
        .expect("recover set members");
    match set_len {
        calod::store::calod_data::DataType::Integer(value) => assert_eq!(value, 2),
        other => panic!("unexpected recovered set length response: {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn snapshot_and_aof_recovery_restore_latest_state() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");

    let mut cfg = CalodConfig::default();
    cfg.persistence.enable_snapshot = true;
    cfg.persistence.enable_aof = true;
    cfg.persistence.snapshot_dir = dir.clone();
    cfg.persistence.aof_dir = dir.clone();

    let store = std::sync::Arc::new(
        ServedCalodStore::try_new_with_config(cfg.clone())
            .await
            .expect("create store"),
    );
    let inner = store.inner_store();

    inner
        .set_cmd("snap:key".to_string(), b"v1".to_vec(), None, None)
        .await
        .expect("seed initial value");
    inner
        .save(dir.to_str().expect("utf8 snapshot path"))
        .await
        .expect("save snapshot");

    let active = std::sync::Arc::new(dashmap::DashMap::with_hasher(ahash::RandomState::new()));
    let metrics = std::sync::Arc::new(calod::cld_srv::metrics::MetricsAggregator::new());
    let auth = std::sync::Arc::new(calod::cld_srv::auth::AuthManager::new(false));
    let conn = std::sync::Arc::new(tokio::sync::Mutex::new(
        calod::cld_srv::connection::Connection::new(
            store.clone(),
            calod::cld_srv::connection::ConnectionConfig::default(),
            metrics,
            auth,
        ),
    ));

    let _ = store
        .execute(
            Command::Set {
                key: "snap:key".to_string(),
                value: b"v2".to_vec(),
                expire_option: None,
                set_option: None,
            },
            active.clone(),
            conn.clone(),
            calod::cld_srv::protocol::ConnCtx::default(),
        )
        .await;
    let _ = store
        .execute(
            Command::Set {
                key: "snap:key2".to_string(),
                value: b"v3".to_vec(),
                expire_option: None,
                set_option: None,
            },
            active,
            conn,
            calod::cld_srv::protocol::ConnCtx::default(),
        )
        .await;
    store.flush_aof().await.expect("flush aof");
    drop(store);

    let recovered = ServedCalodStore::try_new_with_config(cfg)
        .await
        .expect("recover store");
    let inner = recovered.inner_store();

    match inner.get_cmd("snap:key").await.expect("snap:key") {
        calod::store::calod_data::DataType::String(value) => assert_eq!(value, b"v2"),
        other => panic!("unexpected value for snap:key: {other:?}"),
    }

    match inner.get_cmd("snap:key2").await.expect("snap:key2") {
        calod::store::calod_data::DataType::String(value) => assert_eq!(value, b"v3"),
        other => panic!("unexpected value for snap:key2: {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn incomplete_aof_tail_is_recovered_and_truncated() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");

    let mut cfg = CalodConfig::default();
    cfg.persistence.enable_snapshot = false;
    cfg.persistence.enable_aof = true;
    cfg.persistence.aof_dir = dir.clone();
    cfg.persistence.snapshot_dir = dir.clone();

    let store = std::sync::Arc::new(
        ServedCalodStore::try_new_with_config(cfg.clone())
            .await
            .expect("create store"),
    );
    let active = std::sync::Arc::new(dashmap::DashMap::with_hasher(ahash::RandomState::new()));
    let metrics = std::sync::Arc::new(calod::cld_srv::metrics::MetricsAggregator::new());
    let auth = std::sync::Arc::new(calod::cld_srv::auth::AuthManager::new(false));
    let conn = std::sync::Arc::new(tokio::sync::Mutex::new(
        calod::cld_srv::connection::Connection::new(
            store.clone(),
            calod::cld_srv::connection::ConnectionConfig::default(),
            metrics,
            auth,
        ),
    ));

    let _ = store
        .execute(
            Command::Set {
                key: "tail:key".to_string(),
                value: b"ok".to_vec(),
                expire_option: None,
                set_option: None,
            },
            active,
            conn,
            calod::cld_srv::protocol::ConnCtx::default(),
        )
        .await;
    store.flush_aof().await.expect("flush aof");
    drop(store);

    let aof_path = dir.join("appendonly.aof");
    let mut bytes = fs::read(&aof_path).expect("read aof");
    let original_len = bytes.len();
    bytes.extend_from_slice(b"*2\r\n$3\r\nSET\r\n$7\r\nbroken");
    fs::write(&aof_path, &bytes).expect("append truncated tail");

    let recovered = ServedCalodStore::try_new_with_config(cfg)
        .await
        .expect("recover store");
    let inner = recovered.inner_store();

    match inner.get_cmd("tail:key").await.expect("tail:key") {
        calod::store::calod_data::DataType::String(value) => assert_eq!(value, b"ok"),
        other => panic!("unexpected value after truncated-tail recovery: {other:?}"),
    }

    let repaired_len = fs::read(&aof_path).expect("read repaired aof").len();
    assert_eq!(repaired_len, original_len);

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn invalid_non_tail_aof_corruption_still_fails_recovery() {
    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("temp dir created");

    let mut cfg = CalodConfig::default();
    cfg.persistence.enable_snapshot = false;
    cfg.persistence.enable_aof = true;
    cfg.persistence.aof_dir = dir.clone();
    cfg.persistence.snapshot_dir = dir.clone();

    let aof_path = dir.join("appendonly.aof");
    fs::write(
        &aof_path,
        b"*3\r\n$3\r\nSET\r\n$7\r\nbad:key\r\n$2\r\nok\r\nnot-resp\r\n",
    )
    .expect("write corrupt aof");

    match ServedCalodStore::try_new_with_config(cfg).await {
        Ok(_) => panic!("invalid trailing bytes should fail recovery"),
        Err(err) => {
            let err_text = err.to_string();
            assert!(
                err_text.contains("Invalid AOF frame")
                    || err_text.contains("Unable to convert AOF")
            );
        }
    }

    let _ = fs::remove_dir_all(&dir);
}
