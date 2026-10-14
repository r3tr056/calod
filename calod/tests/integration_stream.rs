//! End-to-end stream (X*) coverage via redis-rs — the Phase 8 core subset
//! (XADD/XLEN/XRANGE/XREVRANGE/XDEL/XTRIM/XREAD, no consumer groups yet).

mod common;

use common::TestServer;
use redis::streams::{StreamRangeReply, StreamReadOptions, StreamReadReply};

#[tokio::test]
async fn stream_add_len_range_revrange() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    // Explicit ids so assertions are deterministic.
    let id1: String = redis::cmd("XADD")
        .arg("s")
        .arg("1-1")
        .arg("field")
        .arg("a")
        .query_async(&mut conn)
        .await
        .expect("xadd 1-1");
    assert_eq!(id1, "1-1");

    let id2: String = redis::cmd("XADD")
        .arg("s")
        .arg("2-2")
        .arg("field")
        .arg("b")
        .arg("k")
        .arg("v")
        .query_async(&mut conn)
        .await
        .expect("xadd 2-2");
    assert_eq!(id2, "2-2");

    // An id <= the stream top is rejected.
    let smaller: redis::RedisResult<String> = redis::cmd("XADD")
        .arg("s")
        .arg("1-0")
        .arg("field")
        .arg("x")
        .query_async(&mut conn)
        .await;
    assert!(smaller.is_err(), "id <= last must be rejected");

    let len: i64 = redis::cmd("XLEN")
        .arg("s")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(len, 2);

    // XRANGE - + returns both entries with their field maps, ascending.
    let range: StreamRangeReply = redis::cmd("XRANGE")
        .arg("s")
        .arg("-")
        .arg("+")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(range.ids.len(), 2);
    assert_eq!(range.ids[0].id, "1-1");
    let f0: String = range.ids[0].get("field").unwrap();
    assert_eq!(f0, "a");
    assert_eq!(range.ids[1].id, "2-2");
    let kv: String = range.ids[1].get("k").unwrap();
    assert_eq!(kv, "v");

    // XREVRANGE + - returns them descending.
    let rev: StreamRangeReply = redis::cmd("XREVRANGE")
        .arg("s")
        .arg("+")
        .arg("-")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(rev.ids.len(), 2);
    assert_eq!(rev.ids[0].id, "2-2");
    assert_eq!(rev.ids[1].id, "1-1");

    // COUNT caps the number of entries.
    let one: StreamRangeReply = redis::cmd("XRANGE")
        .arg("s")
        .arg("-")
        .arg("+")
        .arg("COUNT")
        .arg(1)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(one.ids.len(), 1);
    assert_eq!(one.ids[0].id, "1-1");

    server.shutdown().await;
}

#[tokio::test]
async fn stream_autoid_del_trim() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    // Auto-id (`*`) yields a monotonically increasing id each call.
    let a: String = redis::cmd("XADD")
        .arg("a")
        .arg("*")
        .arg("n")
        .arg("1")
        .query_async(&mut conn)
        .await
        .unwrap();
    let b: String = redis::cmd("XADD")
        .arg("a")
        .arg("*")
        .arg("n")
        .arg("2")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(a < b, "auto ids must strictly increase ({a} !< {b})");

    let c: String = redis::cmd("XADD")
        .arg("a")
        .arg("*")
        .arg("n")
        .arg("3")
        .query_async(&mut conn)
        .await
        .unwrap();
    let len: i64 = redis::cmd("XLEN")
        .arg("a")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(len, 3);

    // XDEL removes a specific entry and returns the deleted count.
    let deleted: i64 = redis::cmd("XDEL")
        .arg("a")
        .arg(&b)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    let len2: i64 = redis::cmd("XLEN")
        .arg("a")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(len2, 2);

    // XTRIM MAXLEN keeps only the newest entry (c).
    let trimmed: i64 = redis::cmd("XTRIM")
        .arg("a")
        .arg("MAXLEN")
        .arg(1)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(trimmed, 1);
    let remaining: StreamRangeReply = redis::cmd("XRANGE")
        .arg("a")
        .arg("-")
        .arg("+")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(remaining.ids.len(), 1);
    assert_eq!(remaining.ids[0].id, c);

    server.shutdown().await;
}

#[tokio::test]
async fn stream_xread_returns_new_entries() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let _: String = redis::cmd("XADD")
        .arg("r")
        .arg("1-1")
        .arg("v")
        .arg("one")
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: String = redis::cmd("XADD")
        .arg("r")
        .arg("2-1")
        .arg("v")
        .arg("two")
        .query_async(&mut conn)
        .await
        .unwrap();

    // XREAD from id 0 returns everything; from 1-1 returns only what's after.
    let opts = StreamReadOptions::default();
    let all: StreamReadReply = redis::cmd("XREAD")
        .arg("COUNT")
        .arg(10)
        .arg("STREAMS")
        .arg("r")
        .arg("0")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(all.keys.len(), 1);
    assert_eq!(all.keys[0].key, "r");
    assert_eq!(all.keys[0].ids.len(), 2);

    let after: StreamReadReply = redis::cmd("XREAD")
        .arg("STREAMS")
        .arg("r")
        .arg("1-1")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(after.keys.len(), 1);
    assert_eq!(after.keys[0].ids.len(), 1);
    assert_eq!(after.keys[0].ids[0].id, "2-1");

    // XREAD with `$` (current last id) on an unchanged stream yields nothing,
    // which redis-rs surfaces as a nil reply (empty option).
    let none: Option<StreamReadReply> = redis::cmd("XREAD")
        .arg("STREAMS")
        .arg("r")
        .arg("$")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(none.is_none(), "XREAD $ on an unchanged stream returns nil");

    let _ = opts;
    server.shutdown().await;
}
