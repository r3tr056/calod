mod common;

use calod::cld_srv::command::Command;
use calod::parser::value::Value;
use calod::store::calod_data::DataType;
use calod::store::config::CalodConfig;
use calod::store::sharded_store::ShardedStore;
use common::{bulk_to_string, Harness};

#[tokio::test]
async fn command_list_contains_new_keyspace_commands() {
    let h = Harness::new().await;
    let value = h.exec(Command::CommandList {}).await;
    let Value::Array(items) = value else {
        panic!("expected COMMAND LIST array");
    };

    let commands: Vec<String> = items.iter().filter_map(bulk_to_string).collect();

    for expected in [
        "randomkey",
        "touch",
        "unlink",
        "rename",
        "renamenx",
        "copy",
        "move",
        "expireat",
        "pexpireat",
        "expiretime",
        "pexpiretime",
        "object",
        "wait",
        "setnx",
        "getdel",
        "getex",
        "msetnx",
        "mget",
        "mset",
        "append",
        "getset",
        "sadd",
        "srem",
        "smembers",
        "sismember",
        "scard",
        "spop",
        "srandmember",
    ] {
        assert!(commands.iter().any(|c| c == expected), "missing {expected}");
    }
}

#[tokio::test]
async fn command_info_is_exposed_for_new_keyspace_commands() {
    let h = Harness::new().await;

    for name in [
        "RANDOMKEY",
        "TOUCH",
        "UNLINK",
        "RENAME",
        "RENAMENX",
        "COPY",
        "MOVE",
        "EXPIREAT",
        "PEXPIREAT",
        "EXPIRETIME",
        "PEXPIRETIME",
        "OBJECT",
        "WAIT",
        "SETNX",
        "GETDEL",
        "GETEX",
        "MSETNX",
        "MGET",
        "MSET",
        "APPEND",
        "GETSET",
        "SADD",
        "SREM",
        "SMEMBERS",
        "SISMEMBER",
        "SCARD",
        "SPOP",
        "SRANDMEMBER",
    ] {
        let value = h
            .exec(Command::CommandInfo {
                command_name: name.to_string(),
            })
            .await;

        let Value::Array(items) = value else {
            panic!("expected COMMAND INFO array for {name}");
        };

        let has_name = items.windows(2).any(|pair| {
            matches!(
                pair,
                [Value::BulkString(k), Value::BulkString(v)]
                    if String::from_utf8_lossy(k) == "name"
                        && String::from_utf8_lossy(v) == name.to_lowercase()
            )
        });

        assert!(has_name, "COMMAND INFO missing name field for {name}");
    }
}

#[tokio::test]
async fn command_help_is_exposed_for_new_keyspace_commands() {
    let h = Harness::new().await;

    for name in [
        "RANDOMKEY",
        "TOUCH",
        "UNLINK",
        "RENAME",
        "RENAMENX",
        "COPY",
        "MOVE",
        "EXPIREAT",
        "PEXPIREAT",
        "EXPIRETIME",
        "PEXPIRETIME",
        "OBJECT",
        "WAIT",
        "SETNX",
        "GETDEL",
        "GETEX",
        "MSETNX",
        "MGET",
        "MSET",
        "APPEND",
        "GETSET",
        "SADD",
        "SREM",
        "SMEMBERS",
        "SISMEMBER",
        "SCARD",
        "SPOP",
        "SRANDMEMBER",
    ] {
        let value = h
            .exec(Command::CommandHelp {
                command_name: name.to_string(),
            })
            .await;

        let Value::Array(items) = value else {
            panic!("expected COMMAND HELP array for {name}");
        };

        assert!(
            !items.is_empty(),
            "COMMAND HELP should return rows for {name}"
        );
    }
}

#[tokio::test]
async fn rename_and_renamenx_work_across_shards() {
    let config = CalodConfig {
        shards: 16,
        ..CalodConfig::default()
    };
    let store = ShardedStore::new(config);

    let mut src = None;
    let mut dst = None;
    let mut dst_other = None;

    'outer: for i in 0..500 {
        let src_candidate = format!("phase0:src:{i}");
        let src_idx = store.get_shard_index(src_candidate.as_str());
        for j in 0..500 {
            let dst_candidate = format!("phase0:dst:{i}:{j}");
            let dst_idx = store.get_shard_index(dst_candidate.as_str());
            if dst_idx != src_idx {
                src = Some(src_candidate.clone());
                dst = Some(dst_candidate.clone());

                for k in 0..500 {
                    let other = format!("phase0:other:{i}:{j}:{k}");
                    if store.get_shard_index(other.as_str()) == dst_idx {
                        dst_other = Some(other);
                        break;
                    }
                }

                if dst_other.is_some() {
                    break 'outer;
                }
            }
        }
    }

    let src = src.expect("must find source key");
    let dst = dst.expect("must find destination key");
    let dst_other = dst_other.expect("must find destination-shard companion key");

    store
        .set_cmd(src.clone(), b"v1".to_vec(), None, None)
        .await
        .expect("set source");

    let renamed = store
        .rename_cmd(&src, &dst, false)
        .await
        .expect("rename should succeed");
    assert!(matches!(renamed, DataType::Status(ref s) if s == "OK"));

    match store.get_cmd(&src).await {
        Ok(DataType::Nil) => {}
        Err(calod::store::error::CacheError::KeyNotFound(_)) => {}
        other => panic!("expected missing source after rename, got {other:?}"),
    }

    let new = store
        .get_cmd(&dst)
        .await
        .expect("get destination after rename");
    assert!(matches!(new, DataType::String(ref s) if s == b"v1"));

    store
        .set_cmd(dst_other.clone(), b"busy".to_vec(), None, None)
        .await
        .expect("seed existing destination");
    store
        .set_cmd(src.clone(), b"v2".to_vec(), None, None)
        .await
        .expect("re-seed source for renamenx");

    let renamenx = store
        .rename_cmd(&src, &dst_other, true)
        .await
        .expect("renamenx should return integer status");
    assert!(matches!(renamenx, DataType::Integer(0)));

    let src_still = store
        .get_cmd(&src)
        .await
        .expect("source should remain for renamenx=0");
    assert!(matches!(src_still, DataType::String(ref s) if s == b"v2"));
}

#[tokio::test]
async fn phase0_new_command_smoke_in_store_api() {
    let config = CalodConfig {
        shards: 8,
        ..CalodConfig::default()
    };
    let store = ShardedStore::new(config);

    store
        .set_cmd("phase0:k1".to_string(), b"1".to_vec(), None, None)
        .await
        .expect("set k1");
    store
        .set_cmd("phase0:k2".to_string(), b"2".to_vec(), None, None)
        .await
        .expect("set k2");

    let touch = store
        .touch_cmd(&[
            "phase0:k1".to_string(),
            "phase0:k2".to_string(),
            "missing".to_string(),
        ])
        .await
        .expect("touch");
    assert!(matches!(touch, DataType::Integer(2)));

    let random = store.randomkey_cmd().await.expect("randomkey");
    assert!(matches!(random, DataType::String(_)));

    let unlink = store
        .unlink_cmd(&["phase0:k1".to_string(), "phase0:k2".to_string()])
        .await
        .expect("unlink");
    assert!(matches!(unlink, DataType::Integer(2)));

    let keys = store.keys_cmd("phase0:*").await.expect("keys");
    let DataType::List(keys) = keys else {
        panic!("expected list");
    };
    assert!(keys.is_empty());
}

#[tokio::test]
async fn shard_capacity_is_applied_per_shard() {
    let config = CalodConfig {
        shards: 4,
        shard_capacity: 4,
        ..CalodConfig::default()
    };
    let store = ShardedStore::new(config);
    let mut first = None;
    let mut second = None;

    'outer: for i in 0..1000 {
        let first_candidate = format!("phase0:capacity:{i}");
        let first_idx = store.get_shard_index(first_candidate.as_str());
        for j in i + 1..1000 {
            let second_candidate = format!("phase0:capacity:{j}");
            if store.get_shard_index(second_candidate.as_str()) == first_idx {
                first = Some(first_candidate);
                second = Some(second_candidate);
                break 'outer;
            }
        }
    }

    let first = first.expect("must find first same-shard key");
    let second = second.expect("must find second same-shard key");

    store
        .set_cmd(first, b"one".to_vec(), None, None)
        .await
        .expect("set first same-shard key");
    store
        .set_cmd(second, b"two".to_vec(), None, None)
        .await
        .expect("set second same-shard key");

    let dbsize = store.dbsize_cmd().await.expect("dbsize");
    assert!(matches!(dbsize, DataType::Integer(2)));
}
