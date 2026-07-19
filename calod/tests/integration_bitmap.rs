//! End-to-end bitmap and HyperLogLog coverage via redis-rs.

mod common;

use common::TestServer;

#[tokio::test]
async fn bitmap_set_get_count_and_bitop() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let old: i64 = redis::cmd("SETBIT")
        .arg("b")
        .arg(7)
        .arg(1)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(old, 0);
    let bit: i64 = redis::cmd("GETBIT")
        .arg("b")
        .arg(7)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(bit, 1);
    let unset: i64 = redis::cmd("GETBIT")
        .arg("b")
        .arg(6)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(unset, 0);

    let _: i64 = redis::cmd("SETBIT")
        .arg("b")
        .arg(0)
        .arg(1)
        .query_async(&mut conn)
        .await
        .unwrap();
    let count: i64 = redis::cmd("BITCOUNT")
        .arg("b")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(count, 2, "bits 0 and 7 are set");

    // BITOP AND / OR over two byte strings.
    let _: () = redis::cmd("SET")
        .arg("k1")
        .arg("abc")
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: () = redis::cmd("SET")
        .arg("k2")
        .arg("abd")
        .query_async(&mut conn)
        .await
        .unwrap();
    let len: i64 = redis::cmd("BITOP")
        .arg("AND")
        .arg("kand")
        .arg("k1")
        .arg("k2")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(len, 3);
    let andres: Vec<u8> = redis::cmd("GET")
        .arg("kand")
        .query_async(&mut conn)
        .await
        .unwrap();
    // 'a'&'a'='a', 'b'&'b'='b', 'c'&'d' = 0x63 & 0x64 = 0x60
    assert_eq!(andres, vec![b'a', b'b', 0x60]);

    server.shutdown().await;
}

#[tokio::test]
async fn hyperloglog_add_count_merge() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let changed: i64 = redis::cmd("PFADD")
        .arg("hll")
        .arg("a")
        .arg("b")
        .arg("c")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(changed, 1);

    let count: i64 = redis::cmd("PFCOUNT")
        .arg("hll")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(count, 3);

    // Adding the same members again should not change the registers.
    let again: i64 = redis::cmd("PFADD")
        .arg("hll")
        .arg("a")
        .arg("b")
        .arg("c")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(again, 0);

    // A second HLL, then PFMERGE.
    let _: i64 = redis::cmd("PFADD")
        .arg("hll2")
        .arg("c")
        .arg("d")
        .arg("e")
        .query_async(&mut conn)
        .await
        .unwrap();
    let merged_ok: String = redis::cmd("PFMERGE")
        .arg("dest")
        .arg("hll")
        .arg("hll2")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(merged_ok, "OK");
    // Union of {a,b,c} and {c,d,e} = {a,b,c,d,e} = 5 distinct.
    let merged: i64 = redis::cmd("PFCOUNT")
        .arg("dest")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(merged, 5);

    // PFCOUNT over multiple keys merges on the fly.
    let multi: i64 = redis::cmd("PFCOUNT")
        .arg("hll")
        .arg("hll2")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(multi, 5);

    server.shutdown().await;
}
