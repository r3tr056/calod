//! End-to-end multi-database (Phase 11): SELECT, per-DB isolation, MOVE,
//! FLUSHDB vs FLUSHALL, and out-of-range handling, via redis-rs.

mod common;

use common::TestServer;

#[tokio::test]
async fn databases_are_isolated() {
    let server = TestServer::spawn().await;
    // Per-connection DB chosen via the URL path; redis-rs issues SELECT on connect.
    let c0 = redis::Client::open(format!("redis://{}/0", server.addr)).expect("c0");
    let c1 = redis::Client::open(format!("redis://{}/1", server.addr)).expect("c1");
    let mut db0 = c0.get_multiplexed_async_connection().await.expect("db0");
    let mut db1 = c1.get_multiplexed_async_connection().await.expect("db1");

    let _: () = redis::cmd("SET")
        .arg("k")
        .arg("v1")
        .query_async(&mut db1)
        .await
        .unwrap();

    // The key set in db1 is invisible in db0.
    let in0: Option<String> = redis::cmd("GET")
        .arg("k")
        .query_async(&mut db0)
        .await
        .unwrap();
    assert_eq!(in0, None);
    let in1: Option<String> = redis::cmd("GET")
        .arg("k")
        .query_async(&mut db1)
        .await
        .unwrap();
    assert_eq!(in1.as_deref(), Some("v1"));

    // A same-named key in db0 is independent.
    let _: () = redis::cmd("SET")
        .arg("k")
        .arg("v0")
        .query_async(&mut db0)
        .await
        .unwrap();
    let in0: String = redis::cmd("GET")
        .arg("k")
        .query_async(&mut db0)
        .await
        .unwrap();
    assert_eq!(in0, "v0");
    let in1: String = redis::cmd("GET")
        .arg("k")
        .query_async(&mut db1)
        .await
        .unwrap();
    assert_eq!(in1, "v1");

    // DBSIZE is per-database.
    let s0: i64 = redis::cmd("DBSIZE").query_async(&mut db0).await.unwrap();
    let s1: i64 = redis::cmd("DBSIZE").query_async(&mut db1).await.unwrap();
    assert_eq!(s0, 1);
    assert_eq!(s1, 1);

    server.shutdown().await;
}

#[tokio::test]
async fn select_switches_db_within_a_connection() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(format!("redis://{}", server.addr)).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    // Commands are awaited sequentially so the SELECT applies to what follows.
    let ok: String = redis::cmd("SELECT")
        .arg(2)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(ok, "OK");
    let _: () = redis::cmd("SET")
        .arg("x")
        .arg("two")
        .query_async(&mut conn)
        .await
        .unwrap();

    let _: String = redis::cmd("SELECT")
        .arg(0)
        .query_async(&mut conn)
        .await
        .unwrap();
    let in0: Option<String> = redis::cmd("GET")
        .arg("x")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(in0, None);

    let _: String = redis::cmd("SELECT")
        .arg(2)
        .query_async(&mut conn)
        .await
        .unwrap();
    let in2: String = redis::cmd("GET")
        .arg("x")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(in2, "two");

    // Out-of-range SELECT errors (default is 16 databases: valid 0..=15).
    let bad: redis::RedisResult<String> = redis::cmd("SELECT").arg(99).query_async(&mut conn).await;
    assert!(bad.is_err());

    server.shutdown().await;
}

#[tokio::test]
async fn move_transfers_key_across_dbs() {
    let server = TestServer::spawn().await;
    let c0 = redis::Client::open(format!("redis://{}/0", server.addr)).expect("c0");
    let c1 = redis::Client::open(format!("redis://{}/1", server.addr)).expect("c1");
    let mut db0 = c0.get_multiplexed_async_connection().await.expect("db0");
    let mut db1 = c1.get_multiplexed_async_connection().await.expect("db1");

    let _: () = redis::cmd("SET")
        .arg("mk")
        .arg("payload")
        .query_async(&mut db0)
        .await
        .unwrap();

    let moved: i64 = redis::cmd("MOVE")
        .arg("mk")
        .arg(1)
        .query_async(&mut db0)
        .await
        .unwrap();
    assert_eq!(moved, 1);

    // Gone from db0, present in db1.
    let in0: Option<String> = redis::cmd("GET")
        .arg("mk")
        .query_async(&mut db0)
        .await
        .unwrap();
    assert_eq!(in0, None);
    let in1: String = redis::cmd("GET")
        .arg("mk")
        .query_async(&mut db1)
        .await
        .unwrap();
    assert_eq!(in1, "payload");

    // MOVE again from db0 (no longer there) returns 0.
    let again: i64 = redis::cmd("MOVE")
        .arg("mk")
        .arg(1)
        .query_async(&mut db0)
        .await
        .unwrap();
    assert_eq!(again, 0);

    server.shutdown().await;
}

#[tokio::test]
async fn flushdb_is_scoped_flushall_is_global() {
    let server = TestServer::spawn().await;
    let c0 = redis::Client::open(format!("redis://{}/0", server.addr)).expect("c0");
    let c1 = redis::Client::open(format!("redis://{}/1", server.addr)).expect("c1");
    let mut db0 = c0.get_multiplexed_async_connection().await.expect("db0");
    let mut db1 = c1.get_multiplexed_async_connection().await.expect("db1");

    let _: () = redis::cmd("SET")
        .arg("a")
        .arg("1")
        .query_async(&mut db0)
        .await
        .unwrap();
    let _: () = redis::cmd("SET")
        .arg("b")
        .arg("2")
        .query_async(&mut db1)
        .await
        .unwrap();

    // FLUSHDB on db0 leaves db1 intact.
    let _: String = redis::cmd("FLUSHDB").query_async(&mut db0).await.unwrap();
    let s0: i64 = redis::cmd("DBSIZE").query_async(&mut db0).await.unwrap();
    let s1: i64 = redis::cmd("DBSIZE").query_async(&mut db1).await.unwrap();
    assert_eq!(s0, 0);
    assert_eq!(s1, 1);

    // FLUSHALL clears every database.
    let _: String = redis::cmd("FLUSHALL").query_async(&mut db1).await.unwrap();
    let s1_after: i64 = redis::cmd("DBSIZE").query_async(&mut db1).await.unwrap();
    assert_eq!(s1_after, 0);

    server.shutdown().await;
}
