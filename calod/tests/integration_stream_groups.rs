//! End-to-end stream consumer-group (Phase 8b) coverage via redis-rs:
//! XGROUP / XREADGROUP / XACK / XPENDING / XCLAIM / XAUTOCLAIM / XINFO.

mod common;

use std::collections::HashMap;

use common::TestServer;
use redis::streams::{StreamClaimReply, StreamReadReply};
use redis::{FromRedisValue, Value};

/// `XPENDING key group` summary reply: `[count, min-id, max-id, [[consumer, count], ...]]`.
type PendingSummary = (i64, Option<String>, Option<String>, Option<Vec<(String, String)>>);

async fn seed(conn: &mut redis::aio::MultiplexedConnection, key: &str) {
    for (id, v) in [("1-1", "a"), ("2-1", "b"), ("3-1", "c")] {
        let _: String = redis::cmd("XADD")
            .arg(key)
            .arg(id)
            .arg("f")
            .arg(v)
            .query_async(conn)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn group_create_read_ack_pending_info() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    seed(&mut conn, "g").await;

    let ok: String = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg("g")
        .arg("grp")
        .arg("0")
        .query_async(&mut conn)
        .await
        .expect("xgroup create");
    assert_eq!(ok, "OK");

    // Re-creating the same group is a BUSYGROUP error.
    let dup: redis::RedisResult<String> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg("g")
        .arg("grp")
        .arg("0")
        .query_async(&mut conn)
        .await;
    assert!(dup.is_err(), "duplicate group must error");

    // `>` delivers all three never-delivered entries to consumer c1.
    let read: StreamReadReply = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg("grp")
        .arg("c1")
        .arg("COUNT")
        .arg(10)
        .arg("STREAMS")
        .arg("g")
        .arg(">")
        .query_async(&mut conn)
        .await
        .expect("xreadgroup");
    assert_eq!(read.keys.len(), 1);
    assert_eq!(read.keys[0].ids.len(), 3);
    assert_eq!(read.keys[0].ids[0].id, "1-1");

    // A second `>` read has nothing new -> nil.
    let again: Option<StreamReadReply> = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg("grp")
        .arg("c1")
        .arg("STREAMS")
        .arg("g")
        .arg(">")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(again.is_none());

    let acked: i64 = redis::cmd("XACK")
        .arg("g")
        .arg("grp")
        .arg("1-1")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(acked, 1);

    // XPENDING summary: [count, min, max, [[consumer, count], ...]].
    let (count, min, max, consumers): PendingSummary = redis::cmd("XPENDING")
        .arg("g")
        .arg("grp")
        .query_async(&mut conn)
        .await
        .expect("xpending summary");
    assert_eq!(count, 2);
    assert_eq!(min.as_deref(), Some("2-1"));
    assert_eq!(max.as_deref(), Some("3-1"));
    assert_eq!(consumers.unwrap(), vec![("c1".to_string(), "2".to_string())]);

    // XINFO GROUPS -> one group, pending == 2.
    let groups: Vec<HashMap<String, Value>> = redis::cmd("XINFO")
        .arg("GROUPS")
        .arg("g")
        .query_async(&mut conn)
        .await
        .expect("xinfo groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(
        String::from_redis_value(&groups[0]["name"]).unwrap(),
        "grp"
    );
    assert_eq!(i64::from_redis_value(&groups[0]["pending"]).unwrap(), 2);

    // XINFO STREAM -> length 3, last id 3-1.
    let info: HashMap<String, Value> = redis::cmd("XINFO")
        .arg("STREAM")
        .arg("g")
        .query_async(&mut conn)
        .await
        .expect("xinfo stream");
    assert_eq!(i64::from_redis_value(&info["length"]).unwrap(), 3);
    assert_eq!(
        String::from_redis_value(&info["last-generated-id"]).unwrap(),
        "3-1"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn claim_and_autoclaim_transfer_ownership() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    seed(&mut conn, "k").await;
    let _: String = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg("k")
        .arg("grp")
        .arg("0")
        .query_async(&mut conn)
        .await
        .unwrap();
    // c1 reads everything (all pending under c1).
    let _: StreamReadReply = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg("grp")
        .arg("c1")
        .arg("STREAMS")
        .arg("k")
        .arg(">")
        .query_async(&mut conn)
        .await
        .unwrap();

    // c2 claims two entries (min-idle 0 -> always eligible).
    let claimed: StreamClaimReply = redis::cmd("XCLAIM")
        .arg("k")
        .arg("grp")
        .arg("c2")
        .arg(0)
        .arg("1-1")
        .arg("2-1")
        .query_async(&mut conn)
        .await
        .expect("xclaim");
    assert_eq!(claimed.ids.len(), 2);
    assert_eq!(claimed.ids[0].id, "1-1");

    // JUSTID returns just the ids.
    let justids: Vec<String> = redis::cmd("XCLAIM")
        .arg("k")
        .arg("grp")
        .arg("c2")
        .arg(0)
        .arg("3-1")
        .arg("JUSTID")
        .query_async(&mut conn)
        .await
        .expect("xclaim justid");
    assert_eq!(justids, vec!["3-1".to_string()]);

    // XAUTOCLAIM by c3 sweeps everything from 0 (min-idle 0).
    let (cursor, autoclaimed, deleted): (String, StreamClaimReply, Vec<String>) =
        redis::cmd("XAUTOCLAIM")
            .arg("k")
            .arg("grp")
            .arg("c3")
            .arg(0)
            .arg("0")
            .query_async(&mut conn)
            .await
            .expect("xautoclaim");
    assert_eq!(cursor, "0-0", "scan exhausted");
    assert_eq!(autoclaimed.ids.len(), 3);
    assert!(deleted.is_empty());

    // XINFO CONSUMERS: c3 now owns all 3 pending entries.
    let consumers: Vec<HashMap<String, Value>> = redis::cmd("XINFO")
        .arg("CONSUMERS")
        .arg("k")
        .arg("grp")
        .query_async(&mut conn)
        .await
        .expect("xinfo consumers");
    let c3 = consumers
        .iter()
        .find(|m| String::from_redis_value(&m["name"]).unwrap() == "c3")
        .expect("c3 present");
    assert_eq!(i64::from_redis_value(&c3["pending"]).unwrap(), 3);

    server.shutdown().await;
}
