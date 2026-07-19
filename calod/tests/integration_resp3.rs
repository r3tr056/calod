//! End-to-end RESP3 coverage: a real `redis-rs` client negotiates RESP3 via the
//! `HELLO 3` handshake against an in-process calod and exercises protocol-typed
//! replies (native maps). Also confirms RESP2 and RESP3 clients can both connect
//! to the same server.

mod common;

use std::collections::HashMap;

use common::TestServer;
use redis::AsyncCommands;

#[tokio::test]
async fn resp3_handshake_then_string_roundtrip() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url_resp3()).expect("create resp3 client");
    // get_multiplexed_async_connection performs the HELLO 3 handshake; if calod
    // rejected RESP3 or replied with a malformed handshake map this would error.
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("RESP3 connect / HELLO 3 handshake");

    let _: () = conn.set("resp3:k", "v").await.expect("set");
    let got: String = conn.get("resp3:k").await.expect("get");
    assert_eq!(got, "v");

    server.shutdown().await;
}

#[tokio::test]
async fn resp3_hgetall_decodes_native_map() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url_resp3()).expect("create resp3 client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("RESP3 connect");

    let _: () = redis::cmd("HSET")
        .arg("resp3:h")
        .arg("f1")
        .arg("1")
        .arg("f2")
        .arg("2")
        .query_async(&mut conn)
        .await
        .expect("hset");

    // Under RESP3 calod replies with a `%` map; redis-rs decodes it directly.
    let map: HashMap<String, String> = conn.hgetall("resp3:h").await.expect("hgetall");
    assert_eq!(map.get("f1").map(String::as_str), Some("1"));
    assert_eq!(map.get("f2").map(String::as_str), Some("2"));

    server.shutdown().await;
}

#[tokio::test]
async fn both_resp2_and_resp3_clients_connect() {
    let server = TestServer::spawn().await;

    let c2 = redis::Client::open(server.redis_url()).expect("resp2 client");
    let mut conn2 = c2
        .get_multiplexed_async_connection()
        .await
        .expect("resp2 connect");
    let pong2: String = redis::cmd("PING")
        .query_async(&mut conn2)
        .await
        .expect("resp2 ping");
    assert_eq!(pong2, "PONG");

    let c3 = redis::Client::open(server.redis_url_resp3()).expect("resp3 client");
    let mut conn3 = c3
        .get_multiplexed_async_connection()
        .await
        .expect("resp3 connect");
    let pong3: String = redis::cmd("PING")
        .query_async(&mut conn3)
        .await
        .expect("resp3 ping");
    assert_eq!(pong3, "PONG");

    server.shutdown().await;
}
