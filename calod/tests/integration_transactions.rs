//! End-to-end transaction coverage (MULTI/EXEC/WATCH/DISCARD) via redis-rs.

mod common;

use common::TestServer;

#[tokio::test]
async fn multi_exec_atomic_pipeline_returns_results() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    // redis-rs `.atomic()` wraps the pipeline in MULTI ... EXEC and returns the
    // EXEC result array.
    let (set_res, incr_res): (String, i64) = redis::pipe()
        .atomic()
        .cmd("SET")
        .arg("tx:c")
        .arg("10")
        .cmd("INCR")
        .arg("tx:c")
        .query_async(&mut conn)
        .await
        .expect("MULTI/EXEC pipeline");
    assert_eq!(set_res, "OK");
    assert_eq!(incr_res, 11);

    server.shutdown().await;
}

#[tokio::test]
async fn watch_aborts_exec_when_key_changes() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let _: () = redis::cmd("SET")
        .arg("wk")
        .arg("orig")
        .query_async(&mut conn)
        .await
        .unwrap();

    // WATCH the key, begin a transaction, queue a write.
    let _: () = redis::cmd("WATCH")
        .arg("wk")
        .query_async(&mut conn)
        .await
        .unwrap();
    let multi: String = redis::cmd("MULTI").query_async(&mut conn).await.unwrap();
    assert_eq!(multi, "OK");
    let queued: String = redis::cmd("SET")
        .arg("wk")
        .arg("from-txn")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(queued, "QUEUED");

    // A different connection modifies the watched key.
    let mut other = client
        .get_multiplexed_async_connection()
        .await
        .expect("other connection");
    let _: () = redis::cmd("SET")
        .arg("wk")
        .arg("changed")
        .query_async(&mut other)
        .await
        .unwrap();

    // EXEC must abort (null array -> None) because wk changed since WATCH.
    let exec: Option<Vec<String>> = redis::cmd("EXEC").query_async(&mut conn).await.unwrap();
    assert!(exec.is_none(), "EXEC should abort with a null array");

    // The transaction's write must NOT have been applied.
    let val: String = redis::cmd("GET")
        .arg("wk")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(val, "changed");

    server.shutdown().await;
}

#[tokio::test]
async fn watch_allows_exec_when_unchanged() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let _: () = redis::cmd("WATCH")
        .arg("wk2")
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: () = redis::cmd("MULTI").query_async(&mut conn).await.unwrap();
    let _: String = redis::cmd("SET")
        .arg("wk2")
        .arg("v")
        .query_async(&mut conn)
        .await
        .unwrap();
    // No other client touched wk2, so EXEC applies the queued command.
    let exec: Vec<String> = redis::cmd("EXEC").query_async(&mut conn).await.unwrap();
    assert_eq!(exec, vec!["OK".to_string()]);

    let val: String = redis::cmd("GET")
        .arg("wk2")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(val, "v");

    server.shutdown().await;
}

#[tokio::test]
async fn discard_drops_queued_commands() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let _: () = redis::cmd("MULTI").query_async(&mut conn).await.unwrap();
    let queued: String = redis::cmd("SET")
        .arg("d:k")
        .arg("v")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(queued, "QUEUED");
    let discard: String = redis::cmd("DISCARD").query_async(&mut conn).await.unwrap();
    assert_eq!(discard, "OK");

    // The queued SET was discarded; the key must not exist.
    let exists: i64 = redis::cmd("EXISTS")
        .arg("d:k")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(exists, 0);

    server.shutdown().await;
}
