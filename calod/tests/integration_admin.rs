//! End-to-end admin/introspection (Phase 13) via redis-rs: DEBUG, MEMORY,
//! SLOWLOG, LATENCY, ACL, FUNCTION, CLIENT subcommands, RESET, LOLWUT.

mod common;

use common::TestServer;
use redis::Value;

#[tokio::test]
async fn debug_and_memory_introspection() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let _: () = redis::cmd("SET")
        .arg("k")
        .arg("v")
        .query_async(&mut conn)
        .await
        .unwrap();

    // DEBUG OBJECT returns an encoding description for an existing key.
    let obj: String = redis::cmd("DEBUG")
        .arg("OBJECT")
        .arg("k")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(obj.contains("encoding:"), "got: {obj}");

    // DEBUG OBJECT on a missing key errors.
    let missing: redis::RedisResult<String> = redis::cmd("DEBUG")
        .arg("OBJECT")
        .arg("nope")
        .query_async(&mut conn)
        .await;
    assert!(missing.is_err());

    // DEBUG JMAP / SET-ACTIVE-EXPIRE are accepted no-ops.
    let jmap: String = redis::cmd("DEBUG")
        .arg("JMAP")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(jmap, "OK");
    let sae: String = redis::cmd("DEBUG")
        .arg("SET-ACTIVE-EXPIRE")
        .arg("0")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(sae, "OK");

    // MEMORY USAGE returns an integer for an existing key, nil for a missing one.
    let usage: Option<i64> = redis::cmd("MEMORY")
        .arg("USAGE")
        .arg("k")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(usage.unwrap() > 0);
    let usage_missing: Option<i64> = redis::cmd("MEMORY")
        .arg("USAGE")
        .arg("nope")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(usage_missing, None);

    let doctor: String = redis::cmd("MEMORY")
        .arg("DOCTOR")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(!doctor.is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn slowlog_latency_lolwut() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let len: i64 = redis::cmd("SLOWLOG")
        .arg("LEN")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(len, 0);
    let entries: Vec<Value> = redis::cmd("SLOWLOG")
        .arg("GET")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(entries.is_empty());
    let reset: String = redis::cmd("SLOWLOG")
        .arg("RESET")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(reset, "OK");

    let lat_reset: i64 = redis::cmd("LATENCY")
        .arg("RESET")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(lat_reset, 0);
    let lat_hist: Vec<Value> = redis::cmd("LATENCY")
        .arg("HISTORY")
        .arg("event")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(lat_hist.is_empty());

    let lolwut: String = redis::cmd("LOLWUT").query_async(&mut conn).await.unwrap();
    assert!(!lolwut.is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn acl_function_and_client_subcommands() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let whoami: String = redis::cmd("ACL")
        .arg("WHOAMI")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(whoami, "default");
    let cats: Vec<String> = redis::cmd("ACL")
        .arg("CAT")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(cats.iter().any(|c| c == "read"));
    let list: Vec<String> = redis::cmd("ACL")
        .arg("LIST")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(list[0].contains("default"));

    let flist: Vec<Value> = redis::cmd("FUNCTION")
        .arg("LIST")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(flist.is_empty());

    // CLIENT SETINFO / NO-EVICT / NO-TOUCH are accepted (clients send these).
    let setinfo: String = redis::cmd("CLIENT")
        .arg("SETINFO")
        .arg("lib-name")
        .arg("redis-rs")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(setinfo, "OK");
    let no_evict: String = redis::cmd("CLIENT")
        .arg("NO-EVICT")
        .arg("on")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(no_evict, "OK");

    // WAITAOF with no AOF/replicas -> [0, 0].
    let waitaof: Vec<i64> = redis::cmd("WAITAOF")
        .arg(0)
        .arg(0)
        .arg(0)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(waitaof, vec![0, 0]);

    server.shutdown().await;
}

#[tokio::test]
async fn reset_restores_default_state() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(format!("redis://{}", server.addr)).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    // Switch DB, then RESET should bring us back to DB 0.
    let _: String = redis::cmd("SELECT")
        .arg(3)
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: () = redis::cmd("SET")
        .arg("rk")
        .arg("rv")
        .query_async(&mut conn)
        .await
        .unwrap();

    let reset: String = redis::cmd("RESET").query_async(&mut conn).await.unwrap();
    assert_eq!(reset, "RESET");

    // After RESET we are on DB 0, where "rk" (written in DB 3) does not exist.
    let got: Option<String> = redis::cmd("GET")
        .arg("rk")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(got, None);

    server.shutdown().await;
}
