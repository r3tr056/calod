mod common;

use calod::cld_srv::command::{Command, SetExpireOption, SetOption};
use calod::parser::value::Value;
use common::{bulk_to_string, Harness};

fn info_field<'a>(info: &'a str, field: &str) -> Option<&'a str> {
    info.lines()
        .find_map(|line| line.strip_prefix(&format!("{field}:")))
}

#[tokio::test]
async fn set_get_returns_redis_shapes() {
    let h = Harness::new().await;

    let set = h
        .exec(Command::Set {
            key: "k1".to_string(),
            value: "v1".to_string().into_bytes(),
            expire_option: None,
            set_option: None,
        })
        .await;
    assert_eq!(set, Value::SimpleString("OK".to_string()));

    let get = h
        .exec(Command::Get {
            key: "k1".to_string(),
        })
        .await;
    assert_eq!(bulk_to_string(&get).as_deref(), Some("v1"));
}

#[tokio::test]
async fn info_reports_live_memory_and_command_counters() {
    let h = Harness::new().await;

    let _ = h
        .exec(Command::Set {
            key: "info:key".to_string(),
            value: "info-value".to_string().into_bytes(),
            expire_option: None,
            set_option: None,
        })
        .await;
    let _ = h
        .exec(Command::Get {
            key: "info:key".to_string(),
        })
        .await;

    let info = h.exec(Command::Info { section: None }).await;
    let info = bulk_to_string(&info).expect("INFO should be a bulk string");

    let used_memory: u64 = info_field(&info, "used_memory")
        .expect("used_memory should be reported")
        .parse()
        .expect("used_memory should be numeric");
    let commands: u64 = info_field(&info, "total_commands_processed")
        .expect("total_commands_processed should be reported")
        .parse()
        .expect("total_commands_processed should be numeric");

    assert!(used_memory > 0);
    assert!(commands >= 3);
}

#[tokio::test]
async fn mget_contains_nil_for_missing_keys() {
    let h = Harness::new().await;

    let _ = h
        .exec(Command::Set {
            key: "a".to_string(),
            value: "1".to_string().into_bytes(),
            expire_option: None,
            set_option: None,
        })
        .await;

    let resp = h
        .exec(Command::MGet {
            keys: vec!["a".to_string(), "missing".to_string(), "b".to_string()],
        })
        .await;

    match resp {
        Value::Array(items) => {
            assert_eq!(items.len(), 3);
            assert_eq!(bulk_to_string(&items[0]).as_deref(), Some("1"));
            assert_eq!(items[1], Value::Nil);
            assert_eq!(items[2], Value::Nil);
        }
        other => panic!("expected RESP array, got: {other:?}"),
    }
}

#[tokio::test]
async fn exists_and_del_are_variadic() {
    let h = Harness::new().await;

    for (k, v) in [("x", "10"), ("y", "20"), ("z", "30")] {
        let _ = h
            .exec(Command::Set {
                key: k.to_string(),
                value: v.to_string().into_bytes(),
                expire_option: None,
                set_option: None,
            })
            .await;
    }

    let exists = h
        .exec(Command::Exists {
            keys: vec!["x".to_string(), "y".to_string(), "missing".to_string()],
        })
        .await;
    assert_eq!(exists, Value::Integer(2));

    let del = h
        .exec(Command::Del {
            keys: vec!["x".to_string(), "missing".to_string(), "z".to_string()],
        })
        .await;
    assert_eq!(del, Value::Integer(2));
}

#[tokio::test]
async fn hash_missing_key_semantics_match_redis() {
    let h = Harness::new().await;

    let hget = h
        .exec(Command::HGet {
            key: "h_missing".to_string(),
            field: b"f".to_vec(),
        })
        .await;
    assert_eq!(hget, Value::Nil);

    let hmget = h
        .exec(Command::HMGet {
            key: "h_missing".to_string(),
            fields: vec![b"a".to_vec(), b"b".to_vec()],
        })
        .await;
    match hmget {
        Value::Array(items) => {
            assert_eq!(items, vec![Value::Nil, Value::Nil]);
        }
        other => panic!("expected array, got: {other:?}"),
    }

    let hlen = h
        .exec(Command::HLen {
            key: "h_missing".to_string(),
        })
        .await;
    assert_eq!(hlen, Value::Integer(0));
}

#[tokio::test]
async fn list_missing_key_semantics_match_redis() {
    let h = Harness::new().await;

    let lpop = h
        .exec(Command::LPop {
            key: "l_missing".to_string(),
        })
        .await;
    assert_eq!(lpop, Value::Nil);

    let llen = h
        .exec(Command::LLen {
            key: "l_missing".to_string(),
        })
        .await;
    assert_eq!(llen, Value::Integer(0));

    let lrange = h
        .exec(Command::LRange {
            key: "l_missing".to_string(),
            start: 0,
            end: -1,
        })
        .await;
    match lrange {
        Value::Array(items) => assert!(items.is_empty()),
        other => panic!("expected empty array, got: {other:?}"),
    }
}

#[tokio::test]
async fn lpush_multi_value_order_matches_redis() {
    let h = Harness::new().await;

    let len = h
        .exec(Command::ListLPush {
            key: "l_order".to_string(),
            values: vec![b"a".to_vec(), b"b".to_vec()],
        })
        .await;
    assert_eq!(len, Value::Integer(2));

    let popped = h
        .exec(Command::RPop {
            key: "l_order".to_string(),
        })
        .await;
    assert_eq!(bulk_to_string(&popped).as_deref(), Some("a"));
}

#[tokio::test]
async fn rpoplpush_moves_between_shards() {
    let h = Harness::new().await;

    let _ = h
        .exec(Command::ListRPush {
            key: "rpoplpush:source".to_string(),
            values: vec![b"a".to_vec(), b"b".to_vec()],
        })
        .await;

    let moved = h
        .exec(Command::RPopLPush {
            source: "rpoplpush:source".to_string(),
            destination: "rpoplpush:destination".to_string(),
        })
        .await;
    assert_eq!(bulk_to_string(&moved).as_deref(), Some("b"));

    let destination = h
        .exec(Command::LRange {
            key: "rpoplpush:destination".to_string(),
            start: 0,
            end: -1,
        })
        .await;
    match destination {
        Value::Array(items) => assert_eq!(bulk_to_string(&items[0]).as_deref(), Some("b")),
        other => panic!("expected destination array, got {other:?}"),
    }
}

#[tokio::test]
async fn json_subpath_mutations_preserve_root_document() {
    let h = Harness::new().await;

    let set = h
        .exec(Command::JsonSet {
            key: "json:root".to_string(),
            path: "$".to_string(),
            value: r#"{"num":1,"text":"hi","arr":[1],"obj":{"a":1}}"#.to_string(),
        })
        .await;
    assert_eq!(set, Value::SimpleString("OK".to_string()));

    let _ = h
        .exec(Command::JsonNumIncrBy {
            key: "json:root".to_string(),
            path: "$.num".to_string(),
            increment: 2.0,
        })
        .await;
    let _ = h
        .exec(Command::JsonStrAppend {
            key: "json:root".to_string(),
            path: "$.text".to_string(),
            value: " there".to_string(),
        })
        .await;
    let _ = h
        .exec(Command::JsonArrAppend {
            key: "json:root".to_string(),
            path: "$.arr".to_string(),
            values: vec!["2".to_string()],
        })
        .await;

    let root = h
        .exec(Command::JsonGet {
            key: "json:root".to_string(),
            path: "$".to_string(),
        })
        .await;
    let root = bulk_to_string(&root).expect("JSON root should be bulk string");
    assert!(root.contains(r#""num":3"#));
    assert!(root.contains(r#""text":"hi there"#));
    assert!(root.contains(r#""arr":[1,2]"#));
    assert!(root.contains(r#""obj":{"a":1}"#));
}

#[tokio::test]
async fn graph_property_mutations_are_persistent() {
    let h = Harness::new().await;

    let _ = h
        .exec(Command::GraphCreateNode {
            key: "graph:persist".to_string(),
            node_id: "n1".to_string(),
            properties: vec![],
        })
        .await;
    let _ = h
        .exec(Command::GraphSetNodeProperty {
            key: "graph:persist".to_string(),
            node_id: "n1".to_string(),
            property_key: "color".to_string(),
            property_value: "blue".to_string(),
        })
        .await;

    let node_props = h
        .exec(Command::GraphGetNodeProperties {
            key: "graph:persist".to_string(),
            node_id: "n1".to_string(),
        })
        .await;
    let node_props = bulk_to_string(&node_props).expect("node properties should be JSON");
    assert!(node_props.contains(r#""color":"blue"#));
}

#[tokio::test]
async fn nx_xx_set_options_behave() {
    let h = Harness::new().await;

    let _ = h
        .exec(Command::Set {
            key: "opt".to_string(),
            value: "first".to_string().into_bytes(),
            expire_option: None,
            set_option: Some(SetOption::NX),
        })
        .await;

    let _ = h
        .exec(Command::Set {
            key: "opt".to_string(),
            value: "second".to_string().into_bytes(),
            expire_option: None,
            set_option: Some(SetOption::NX),
        })
        .await;

    let get = h
        .exec(Command::Get {
            key: "opt".to_string(),
        })
        .await;
    assert_eq!(bulk_to_string(&get).as_deref(), Some("first"));

    let _ = h
        .exec(Command::Set {
            key: "opt".to_string(),
            value: "third".to_string().into_bytes(),
            expire_option: None,
            set_option: Some(SetOption::XX),
        })
        .await;
    let get2 = h
        .exec(Command::Get {
            key: "opt".to_string(),
        })
        .await;
    assert_eq!(bulk_to_string(&get2).as_deref(), Some("third"));
}

#[tokio::test]
async fn expire_and_ttl_basics() {
    let h = Harness::new().await;

    let _ = h
        .exec(Command::Set {
            key: "ttl-key".to_string(),
            value: "v".to_string().into_bytes(),
            expire_option: Some(SetExpireOption::EX(2)),
            set_option: None,
        })
        .await;

    let ttl = h
        .exec(Command::Ttl {
            key: "ttl-key".to_string(),
        })
        .await;
    match ttl {
        Value::Integer(v) => assert!((0..=2).contains(&v)),
        other => panic!("expected integer ttl, got: {other:?}"),
    }
}

#[tokio::test]
async fn blocking_pop_timeout_is_null_array_not_null_bulk() {
    let h = Harness::new().await;

    // BLPOP on a missing key with a zero timeout returns immediately. Redis
    // frames this as a null ARRAY (`*-1` in RESP2), not a null bulk (`$-1`),
    // because the success reply is a [key, value] array. The harness parses the
    // raw bytes, so `*-1` must come back as Value::NilArray.
    let blpop = h
        .exec(Command::BLPop {
            keys: vec!["bpop:missing".to_string()],
            timeout: 0.0,
        })
        .await;
    assert_eq!(blpop, Value::NilArray, "BLPOP timeout must be a null array");

    let brpop = h
        .exec(Command::BRPop {
            keys: vec!["bpop:missing".to_string()],
            timeout: 0.0,
        })
        .await;
    assert_eq!(brpop, Value::NilArray, "BRPOP timeout must be a null array");

    // Sanity: a missing GET is still a null *bulk* string, distinct from above.
    let get = h
        .exec(Command::Get {
            key: "bpop:missing".to_string(),
        })
        .await;
    assert_eq!(get, Value::Nil, "GET miss must be a null bulk string");
}

#[tokio::test]
async fn unknown_command_is_resp_error() {
    let h = Harness::new().await;
    let v = h.exec(Command::Unknown("NONEXISTENT".to_string())).await;
    match v {
        Value::Error(msg) => assert!(msg.contains("unknown command")),
        other => panic!("expected error, got: {other:?}"),
    }
}

#[tokio::test]
async fn error_text_is_redis_like_for_key_not_found_and_invalid_args() {
    let h = Harness::new().await;

    let missing_get = h
        .exec(Command::Get {
            key: "missing-key-for-error-shape".to_string(),
        })
        .await;
    assert_eq!(missing_get, Value::Nil);

    // 99 is beyond the configured database count (default 16), so COPY rejects
    // it with the canonical out-of-range error.
    let copy_bad_db = h
        .exec(Command::Copy {
            source: "src".to_string(),
            destination: "dst".to_string(),
            destination_db: Some(99),
            replace: false,
        })
        .await;
    match copy_bad_db {
        Value::Error(msg) => assert!(msg.contains("DB index is out of range")),
        other => panic!("expected error, got: {other:?}"),
    }
}

#[tokio::test]
async fn parser_errors_are_returned_without_unknown_command_wrapping() {
    let server = common::TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("create redis client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect to test server");

    let err = redis::cmd("MSETNX")
        .query_async::<()>(&mut conn)
        .await
        .expect_err("MSETNX with no args should fail");
    assert!(err
        .to_string()
        .contains("MSETNX command expects key value pairs"));
    assert!(!err.to_string().contains("unknown command"));

    server.shutdown().await;
}

#[tokio::test]
async fn generic_commands_rename_touch_unlink_randomkey_work() {
    let h = Harness::new().await;

    let random_empty = h.exec(Command::RandomKey {}).await;
    assert_eq!(random_empty, Value::Nil);

    let _ = h
        .exec(Command::Set {
            key: "rk:1".to_string(),
            value: "v1".to_string().into_bytes(),
            expire_option: None,
            set_option: None,
        })
        .await;
    let _ = h
        .exec(Command::Set {
            key: "rk:2".to_string(),
            value: "v2".to_string().into_bytes(),
            expire_option: None,
            set_option: None,
        })
        .await;

    let random_non_empty = h.exec(Command::RandomKey {}).await;
    match random_non_empty {
        Value::BulkString(bs) => {
            let key = String::from_utf8_lossy(&bs).to_string();
            assert!(key == "rk:1" || key == "rk:2");
        }
        other => panic!("expected bulk string, got: {other:?}"),
    }

    let touch = h
        .exec(Command::Touch {
            keys: vec![
                "rk:1".to_string(),
                "rk:2".to_string(),
                "missing".to_string(),
            ],
        })
        .await;
    assert_eq!(touch, Value::Integer(2));

    let rename = h
        .exec(Command::Rename {
            key: "rk:1".to_string(),
            new_key: "rk:3".to_string(),
        })
        .await;
    assert_eq!(rename, Value::SimpleString("OK".to_string()));

    let renamed_value = h
        .exec(Command::Get {
            key: "rk:3".to_string(),
        })
        .await;
    assert_eq!(bulk_to_string(&renamed_value).as_deref(), Some("v1"));

    let renamenx_fail = h
        .exec(Command::RenameNx {
            key: "rk:2".to_string(),
            new_key: "rk:3".to_string(),
        })
        .await;
    assert_eq!(renamenx_fail, Value::Integer(0));

    let same_key_rename = h
        .exec(Command::Rename {
            key: "rk:3".to_string(),
            new_key: "rk:3".to_string(),
        })
        .await;
    match same_key_rename {
        Value::Error(msg) => {
            assert!(msg.contains("source and destination objects are the same"))
        }
        other => panic!("expected error, got: {other:?}"),
    }

    let unlink = h
        .exec(Command::Unlink {
            keys: vec![
                "rk:2".to_string(),
                "rk:3".to_string(),
                "missing".to_string(),
            ],
        })
        .await;
    assert_eq!(unlink, Value::Integer(2));
}
