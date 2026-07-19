mod common;

use calod::cld_srv::command::Command;
use calod::parser::value::Value;
use common::{bulk_to_string, Harness};

/// C1 acceptance test: values must be binary-safe. A blob containing NUL bytes,
/// 0xFF, and other non-UTF-8 sequences must round-trip through SET/GET byte-for-byte
/// identical, exactly as real Redis does. Before the `String`->`Vec<u8>` refactor
/// this corrupted the value via `from_utf8_lossy`.
#[tokio::test]
async fn set_and_get_are_binary_safe() {
    let h = Harness::new().await;

    // Includes embedded NUL, all high bytes, and a lone 0xFF (invalid UTF-8).
    let blob: Vec<u8> = vec![
        0x00, 0x01, 0x02, 0xFF, 0xFE, b'h', b'i', 0x00, 0x80, 0xC0, 0xC1, 0x7F, 0x0A, 0x0D,
    ];

    let set = h
        .exec(Command::Set {
            key: "bin:key".to_string(),
            value: blob.clone(),
            expire_option: None,
            set_option: None,
        })
        .await;
    assert_eq!(set, Value::SimpleString("OK".to_string()));

    let got = h
        .exec(Command::Get {
            key: "bin:key".to_string(),
        })
        .await;
    match got {
        Value::BulkString(bytes) => {
            assert_eq!(bytes, blob, "binary value was corrupted on round-trip")
        }
        other => panic!("expected bulk string, got {other:?}"),
    }

    // STRLEN must count raw bytes, not UTF-8 chars.
    let strlen = h
        .exec(Command::StrLen {
            key: "bin:key".to_string(),
        })
        .await;
    assert_eq!(strlen, Value::Integer(blob.len() as i64));

    // APPEND must be byte-concatenation, also binary-safe.
    let more: Vec<u8> = vec![0x00, 0xAB, 0xCD];
    let _ = h
        .exec(Command::Append {
            key: "bin:key".to_string(),
            value: more.clone(),
        })
        .await;
    let got2 = h
        .exec(Command::Get {
            key: "bin:key".to_string(),
        })
        .await;
    let mut expected = blob.clone();
    expected.extend_from_slice(&more);
    match got2 {
        Value::BulkString(bytes) => assert_eq!(bytes, expected),
        other => panic!("expected bulk string, got {other:?}"),
    }
}

/// C1 acceptance test for collections: List elements, Set members, and Hash
/// fields/values must round-trip arbitrary (non-UTF-8) bytes verbatim, exactly
/// like real Redis. Before the `String`->`Vec<u8>` collection refactor these
/// corrupted binary data via `from_utf8_lossy`.
#[tokio::test]
async fn collections_are_binary_safe() {
    let h = Harness::new().await;
    // Embedded NUL, 0xFF/0xFE (invalid UTF-8), high bytes.
    let blob: Vec<u8> = vec![0x00, 0xFF, 0xFE, b'h', b'i', 0x00, 0x80, 0xC0, 0xC1];

    // List: RPUSH then LRANGE returns the element verbatim.
    let _ = h
        .exec(Command::ListRPush {
            key: "bin:list".to_string(),
            values: vec![blob.clone()],
        })
        .await;
    let lrange = h
        .exec(Command::LRange {
            key: "bin:list".to_string(),
            start: 0,
            end: -1,
        })
        .await;
    match lrange {
        Value::Array(items) => {
            assert_eq!(items, vec![Value::BulkString(blob.clone())]);
        }
        other => panic!("expected array, got {other:?}"),
    }

    // Set: SADD then SMEMBERS returns the member verbatim.
    let _ = h
        .exec(Command::SAdd {
            key: "bin:set".to_string(),
            members: vec![blob.clone()],
        })
        .await;
    let smembers = h
        .exec(Command::SMembers {
            key: "bin:set".to_string(),
        })
        .await;
    match smembers {
        Value::Array(items) => {
            assert!(items.iter().any(|v| *v == Value::BulkString(blob.clone())));
        }
        other => panic!("expected array, got {other:?}"),
    }

    // Hash: binary field AND value round-trip through HSET/HGET.
    let field: Vec<u8> = vec![0x01, 0x00, 0x02, 0xFF];
    let _ = h
        .exec(Command::HSet {
            key: "bin:hash".to_string(),
            field_values: vec![(field.clone(), blob.clone())],
        })
        .await;
    let hget = h
        .exec(Command::HGet {
            key: "bin:hash".to_string(),
            field: field.clone(),
        })
        .await;
    assert_eq!(hget, Value::BulkString(blob.clone()));
}

#[tokio::test]
async fn supports_setex_psetex_and_pttl() {
    let h = Harness::new().await;

    let setex = h
        .exec(Command::SetEx {
            key: "k".to_string(),
            seconds: 2,
            value: "v".to_string().into_bytes(),
        })
        .await;
    assert_eq!(setex, Value::SimpleString("OK".to_string()));

    let pttl = h
        .exec(Command::Pttl {
            key: "k".to_string(),
        })
        .await;
    match pttl {
        Value::Integer(v) => assert!(v >= 0),
        other => panic!("expected integer, got {other:?}"),
    }

    let psetex = h
        .exec(Command::PSetEx {
            key: "k2".to_string(),
            milliseconds: 3000,
            value: "v2".to_string().into_bytes(),
        })
        .await;
    assert_eq!(psetex, Value::SimpleString("OK".to_string()));

    let get = h
        .exec(Command::Get {
            key: "k2".to_string(),
        })
        .await;
    assert_eq!(bulk_to_string(&get).as_deref(), Some("v2"));
}

#[tokio::test]
async fn supports_expireat_pexpireat_and_expiretime_family() {
    let h = Harness::new().await;

    let _ = h
        .exec(Command::Set {
            key: "t_abs".to_string(),
            value: "v".to_string().into_bytes(),
            expire_option: None,
            set_option: None,
        })
        .await;

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_secs();
    let expire_at = now_secs + 2;

    let expireat = h
        .exec(Command::ExpireAt {
            key: "t_abs".to_string(),
            timestamp: expire_at,
        })
        .await;
    assert_eq!(expireat, Value::Integer(1));

    let expiretime = h
        .exec(Command::ExpireTime {
            key: "t_abs".to_string(),
        })
        .await;
    match expiretime {
        Value::Integer(v) => assert!(v >= now_secs as i64),
        other => panic!("expected integer, got {other:?}"),
    }

    let pexpiretime = h
        .exec(Command::PExpireTime {
            key: "t_abs".to_string(),
        })
        .await;
    match pexpiretime {
        Value::Integer(v) => assert!(v > 0),
        other => panic!("expected integer, got {other:?}"),
    }

    let _ = h
        .exec(Command::Set {
            key: "t_abs_ms".to_string(),
            value: "v".to_string().into_bytes(),
            expire_option: None,
            set_option: None,
        })
        .await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_millis() as u64;
    let pexpire_at = now_ms + 1500;

    let pexpireat = h
        .exec(Command::PExpireAt {
            key: "t_abs_ms".to_string(),
            milliseconds_timestamp: pexpire_at,
        })
        .await;
    assert_eq!(pexpireat, Value::Integer(1));

    let pexpiretime2 = h
        .exec(Command::PExpireTime {
            key: "t_abs_ms".to_string(),
        })
        .await;
    match pexpiretime2 {
        Value::Integer(v) => assert!(v >= now_ms as i64),
        other => panic!("expected integer, got {other:?}"),
    }

    let no_ttl = h
        .exec(Command::Set {
            key: "no_ttl_key".to_string(),
            value: "v".to_string().into_bytes(),
            expire_option: None,
            set_option: None,
        })
        .await;
    assert_eq!(no_ttl, Value::SimpleString("OK".to_string()));

    let expiretime_missing_ttl = h
        .exec(Command::ExpireTime {
            key: "no_ttl_key".to_string(),
        })
        .await;
    assert_eq!(expiretime_missing_ttl, Value::Integer(-1));

    let missing_key = h
        .exec(Command::ExpireTime {
            key: "no_such_key".to_string(),
        })
        .await;
    assert_eq!(missing_key, Value::Integer(-2));

    let expire_missing = h
        .exec(Command::Expire {
            key: "no_such_key".to_string(),
            seconds: 60,
        })
        .await;
    assert_eq!(expire_missing, Value::Integer(0));

    let pexpire_missing = h
        .exec(Command::PExpire {
            key: "no_such_key".to_string(),
            milliseconds: 60000,
        })
        .await;
    assert_eq!(pexpire_missing, Value::Integer(0));
}

#[tokio::test]
async fn supports_scan_dbsize_and_flush() {
    let h = Harness::new().await;

    for i in 0..15 {
        let _ = h
            .exec(Command::Set {
                key: format!("scan:{i}"),
                value: i.to_string().into_bytes(),
                expire_option: None,
                set_option: None,
            })
            .await;
    }

    let dbsize = h.exec(Command::DbSize {}).await;
    match dbsize {
        Value::Integer(v) => assert!(v >= 15),
        other => panic!("expected integer, got {other:?}"),
    }

    let scan = h
        .exec(Command::Scan {
            cursor: 0,
            pattern: Some("scan:*".to_string()),
            count: Some(5),
        })
        .await;
    match scan {
        Value::Array(items) => {
            assert_eq!(items.len(), 2);
            assert!(matches!(&items[0], Value::BulkString(_)));
            assert!(matches!(&items[1], Value::Array(_)));
        }
        other => panic!("expected scan array, got {other:?}"),
    }

    let flush = h.exec(Command::FlushDb {}).await;
    assert_eq!(flush, Value::SimpleString("OK".to_string()));

    let dbsize_after = h.exec(Command::DbSize {}).await;
    assert_eq!(dbsize_after, Value::Integer(0));
}

#[tokio::test]
async fn supports_select_valid_and_out_of_range() {
    let h = Harness::new().await;

    // Databases 0..=15 exist (default 16); selecting any of them succeeds.
    let select0 = h.exec(Command::Select { db: 0 }).await;
    assert_eq!(select0, Value::SimpleString("OK".to_string()));
    let select1 = h.exec(Command::Select { db: 1 }).await;
    assert_eq!(select1, Value::SimpleString("OK".to_string()));

    // Out-of-range selection is rejected.
    let select_bad = h.exec(Command::Select { db: 99 }).await;
    match select_bad {
        Value::Error(msg) => assert!(msg.contains("DB index is out of range")),
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn supports_ping_with_message_quit_and_hello() {
    let h = Harness::new().await;

    let ping = h.exec(Command::Ping {}).await;
    assert_eq!(ping, Value::SimpleString("PONG".to_string()));

    let ping_msg = h
        .exec(Command::Echo {
            message: "hello".to_string(),
        })
        .await;
    match ping_msg {
        Value::BulkString(bs) => assert_eq!(String::from_utf8_lossy(&bs), "hello"),
        other => panic!("expected bulk string, got {other:?}"),
    }

    let quit = h.exec(Command::Quit {}).await;
    assert_eq!(quit, Value::SimpleString("OK".to_string()));

    let hello = h
        .exec(Command::Hello {
            protover: Some(2),
            auth: None,
            setname: None,
        })
        .await;
    match hello {
        // RESP2: the handshake map is rendered as a flat array of key/value pairs.
        Value::Array(items) => assert!(!items.is_empty()),
        other => panic!("expected array, got {other:?}"),
    }

    // RESP3 is now negotiated successfully and the reply switches to a native map.
    let hello3 = h
        .exec(Command::Hello {
            protover: Some(3),
            auth: None,
            setname: None,
        })
        .await;
    match hello3 {
        Value::Map(pairs) => assert!(!pairs.is_empty()),
        other => panic!("expected RESP3 map, got {other:?}"),
    }

    // An unsupported version is still rejected with NOPROTO.
    let hello_bad = h
        .exec(Command::Hello {
            protover: Some(4),
            auth: None,
            setname: None,
        })
        .await;
    match hello_bad {
        Value::Error(msg) => assert!(msg.contains("NOPROTO")),
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn supports_copy_move_object_wait_basics() {
    let h = Harness::new().await;

    let _ = h
        .exec(Command::Set {
            key: "csrc".to_string(),
            value: "v1".to_string().into_bytes(),
            expire_option: None,
            set_option: None,
        })
        .await;

    let copy = h
        .exec(Command::Copy {
            source: "csrc".to_string(),
            destination: "cdst".to_string(),
            destination_db: None,
            replace: false,
        })
        .await;
    assert_eq!(copy, Value::Integer(1));

    let copy_second = h
        .exec(Command::Copy {
            source: "csrc".to_string(),
            destination: "cdst".to_string(),
            destination_db: None,
            replace: false,
        })
        .await;
    assert_eq!(copy_second, Value::Integer(0));

    let copy_replace = h
        .exec(Command::Copy {
            source: "csrc".to_string(),
            destination: "cdst".to_string(),
            destination_db: None,
            replace: true,
        })
        .await;
    assert_eq!(copy_replace, Value::Integer(1));

    let get_dst = h
        .exec(Command::Get {
            key: "cdst".to_string(),
        })
        .await;
    match get_dst {
        Value::BulkString(bs) => assert_eq!(String::from_utf8_lossy(&bs), "v1"),
        other => panic!("expected bulk string, got {other:?}"),
    }

    let mv = h
        .exec(Command::Move {
            key: "csrc".to_string(),
            db: 0,
        })
        .await;
    match mv {
        Value::Error(msg) => assert!(msg.contains("source and destination objects are the same")),
        other => panic!("expected error, got {other:?}"),
    }

    // 99 is beyond the configured database count (default 16) -> out of range.
    let mv_bad_db = h
        .exec(Command::Move {
            key: "csrc".to_string(),
            db: 99,
        })
        .await;
    match mv_bad_db {
        Value::Error(msg) => assert!(msg.contains("DB index is out of range")),
        other => panic!("expected error, got {other:?}"),
    }

    let copy_bad_db = h
        .exec(Command::Copy {
            source: "csrc".to_string(),
            destination: "cdbbad".to_string(),
            destination_db: Some(99),
            replace: false,
        })
        .await;
    match copy_bad_db {
        Value::Error(msg) => assert!(msg.contains("DB index is out of range")),
        other => panic!("expected error, got {other:?}"),
    }

    let copy_missing_source = h
        .exec(Command::Copy {
            source: "missing-src".to_string(),
            destination: "cdst2".to_string(),
            destination_db: None,
            replace: false,
        })
        .await;
    assert_eq!(copy_missing_source, Value::Integer(0));

    let obj_encoding = h
        .exec(Command::ObjectEncoding {
            key: "cdst".to_string(),
        })
        .await;
    match obj_encoding {
        Value::BulkString(bs) => assert!(!bs.is_empty()),
        Value::Nil => panic!("expected encoding response for existing key"),
        other => panic!("expected bulk string, got {other:?}"),
    }

    let obj_refcount = h
        .exec(Command::ObjectRefCount {
            key: "cdst".to_string(),
        })
        .await;
    assert_eq!(obj_refcount, Value::Integer(1));

    let obj_idletime = h
        .exec(Command::ObjectIdleTime {
            key: "cdst".to_string(),
        })
        .await;
    match obj_idletime {
        Value::Integer(v) => assert!(v >= 0),
        other => panic!("expected integer, got {other:?}"),
    }

    let obj_missing = h
        .exec(Command::ObjectEncoding {
            key: "missing-object-key".to_string(),
        })
        .await;
    assert_eq!(obj_missing, Value::Nil);

    let wait = h
        .exec(Command::Wait {
            numreplicas: 1,
            timeout_ms: 100,
        })
        .await;
    assert_eq!(wait, Value::Integer(0));
}

#[tokio::test]
async fn supports_setnx_getdel_getex_and_msetnx() {
    let h = Harness::new().await;

    let setnx1 = h
        .exec(Command::SetNx {
            key: "snx".to_string(),
            value: "v1".to_string().into_bytes(),
        })
        .await;
    assert_eq!(setnx1, Value::Integer(1));

    let setnx2 = h
        .exec(Command::SetNx {
            key: "snx".to_string(),
            value: "v2".to_string().into_bytes(),
        })
        .await;
    assert_eq!(setnx2, Value::Integer(0));

    let getdel = h
        .exec(Command::GetDel {
            key: "snx".to_string(),
        })
        .await;
    match getdel {
        Value::BulkString(bs) => assert_eq!(String::from_utf8_lossy(&bs), "v1"),
        other => panic!("expected bulk string, got {other:?}"),
    }

    let getdel_missing = h
        .exec(Command::GetDel {
            key: "snx".to_string(),
        })
        .await;
    assert_eq!(getdel_missing, Value::Nil);

    let _ = h
        .exec(Command::Set {
            key: "gex".to_string(),
            value: "gx".to_string().into_bytes(),
            expire_option: None,
            set_option: None,
        })
        .await;

    let getex = h
        .exec(Command::GetEx {
            key: "gex".to_string(),
            option: Some(calod::cld_srv::command::GetExOption::EX(2)),
        })
        .await;
    match getex {
        Value::BulkString(bs) => assert_eq!(String::from_utf8_lossy(&bs), "gx"),
        other => panic!("expected bulk string, got {other:?}"),
    }

    let ttl = h
        .exec(Command::Ttl {
            key: "gex".to_string(),
        })
        .await;
    match ttl {
        Value::Integer(v) => assert!((0..=2).contains(&v)),
        other => panic!("expected integer ttl, got {other:?}"),
    }

    let _ = h
        .exec(Command::Set {
            key: "gex_persist".to_string(),
            value: "gp".to_string().into_bytes(),
            expire_option: Some(calod::cld_srv::command::SetExpireOption::EX(60)),
            set_option: None,
        })
        .await;
    let getex_persist = h
        .exec(Command::GetEx {
            key: "gex_persist".to_string(),
            option: Some(calod::cld_srv::command::GetExOption::Persist),
        })
        .await;
    assert_eq!(bulk_to_string(&getex_persist).as_deref(), Some("gp"));
    let ttl_persisted = h
        .exec(Command::Ttl {
            key: "gex_persist".to_string(),
        })
        .await;
    assert_eq!(ttl_persisted, Value::Integer(-1));

    let _ = h
        .exec(Command::Set {
            key: "mx1".to_string(),
            value: "a".to_string().into_bytes(),
            expire_option: None,
            set_option: None,
        })
        .await;

    let msetnx_fail = h
        .exec(Command::MSetNx {
            key_values: vec![
                ("mx1".to_string(), "x".to_string().into_bytes()),
                ("mx2".to_string(), "y".to_string().into_bytes()),
            ],
        })
        .await;
    assert_eq!(msetnx_fail, Value::Integer(0));

    let msetnx_ok = h
        .exec(Command::MSetNx {
            key_values: vec![
                ("mx3".to_string(), "x".to_string().into_bytes()),
                ("mx4".to_string(), "y".to_string().into_bytes()),
            ],
        })
        .await;
    assert_eq!(msetnx_ok, Value::Integer(1));

    let msetnx_bad = h.exec(Command::MSetNx { key_values: vec![] }).await;
    match msetnx_bad {
        Value::Error(msg) => assert!(msg.contains("wrong number of arguments")),
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn supports_set_family_basics() {
    let h = Harness::new().await;

    let sadd = h
        .exec(Command::SAdd {
            key: "set:1".to_string(),
            members: vec![b"a".to_vec(), b"b".to_vec(), b"a".to_vec()],
        })
        .await;
    assert_eq!(sadd, Value::Integer(2));

    let scard = h
        .exec(Command::SCard {
            key: "set:1".to_string(),
        })
        .await;
    assert_eq!(scard, Value::Integer(2));

    let sismember_yes = h
        .exec(Command::SIsMember {
            key: "set:1".to_string(),
            member: b"a".to_vec(),
        })
        .await;
    assert_eq!(sismember_yes, Value::Integer(1));

    let sismember_no = h
        .exec(Command::SIsMember {
            key: "set:1".to_string(),
            member: b"z".to_vec(),
        })
        .await;
    assert_eq!(sismember_no, Value::Integer(0));

    let smembers = h
        .exec(Command::SMembers {
            key: "set:1".to_string(),
        })
        .await;
    match smembers {
        Value::Array(items) => {
            let vals = items
                .iter()
                .filter_map(|v| match v {
                    Value::BulkString(bs) => Some(String::from_utf8_lossy(bs).to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert!(vals.contains(&"a".to_string()));
            assert!(vals.contains(&"b".to_string()));
        }
        other => panic!("expected array, got {other:?}"),
    }

    let srand = h
        .exec(Command::SRandMember {
            key: "set:1".to_string(),
            count: None,
        })
        .await;
    match srand {
        Value::BulkString(_) => {}
        other => panic!("expected bulk string, got {other:?}"),
    }

    let spop = h
        .exec(Command::SPop {
            key: "set:1".to_string(),
            count: Some(1),
        })
        .await;
    match spop {
        Value::Array(items) => assert_eq!(items.len(), 1),
        other => panic!("expected array, got {other:?}"),
    }

    let srem = h
        .exec(Command::SRem {
            key: "set:1".to_string(),
            members: vec![b"b".to_vec(), b"x".to_vec()],
        })
        .await;
    match srem {
        Value::Integer(v) => assert!((0..=1).contains(&v)),
        other => panic!("expected integer, got {other:?}"),
    }

    let sr_multi = h
        .exec(Command::SRandMember {
            key: "set:1".to_string(),
            count: Some(2),
        })
        .await;
    match sr_multi {
        Value::Array(items) => assert!(items.len() <= 2),
        other => panic!("expected array, got {other:?}"),
    }

    let sr_multi_neg = h
        .exec(Command::SRandMember {
            key: "set:1".to_string(),
            count: Some(-3),
        })
        .await;
    match sr_multi_neg {
        Value::Array(items) => assert!(items.len() <= 3),
        other => panic!("expected array, got {other:?}"),
    }
}
