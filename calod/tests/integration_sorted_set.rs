//! End-to-end sorted-set (ZSET) coverage via redis-rs.

mod common;

use common::TestServer;

#[tokio::test]
async fn zset_core_operations() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let added: i64 = redis::cmd("ZADD")
        .arg("z")
        .arg(1.0)
        .arg("a")
        .arg(2.0)
        .arg("b")
        .arg(3.0)
        .arg("c")
        .query_async(&mut conn)
        .await
        .expect("zadd");
    assert_eq!(added, 3);

    let card: i64 = redis::cmd("ZCARD")
        .arg("z")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(card, 3);

    let score: f64 = redis::cmd("ZSCORE")
        .arg("z")
        .arg("b")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(score, 2.0);

    let range: Vec<String> = redis::cmd("ZRANGE")
        .arg("z")
        .arg(0)
        .arg(-1)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(range, vec!["a", "b", "c"]);

    let with_scores: Vec<(String, f64)> = redis::cmd("ZRANGE")
        .arg("z")
        .arg(0)
        .arg(-1)
        .arg("WITHSCORES")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(
        with_scores,
        vec![("a".into(), 1.0), ("b".into(), 2.0), ("c".into(), 3.0)]
    );

    let rev: Vec<String> = redis::cmd("ZREVRANGE")
        .arg("z")
        .arg(0)
        .arg(-1)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(rev, vec!["c", "b", "a"]);

    let rank: i64 = redis::cmd("ZRANK")
        .arg("z")
        .arg("c")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(rank, 2);

    let new_score: f64 = redis::cmd("ZINCRBY")
        .arg("z")
        .arg(10.0)
        .arg("a")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(new_score, 11.0);

    let by_score: Vec<String> = redis::cmd("ZRANGEBYSCORE")
        .arg("z")
        .arg(2)
        .arg(3)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(by_score, vec!["b", "c"]);

    let count: i64 = redis::cmd("ZCOUNT")
        .arg("z")
        .arg(2)
        .arg(3)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(count, 2);

    let removed: i64 = redis::cmd("ZREM")
        .arg("z")
        .arg("b")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(removed, 1);

    // After removing b, members are c=3 and a=11; ZPOPMIN pops c.
    let popped: Vec<String> = redis::cmd("ZPOPMIN")
        .arg("z")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(popped, vec!["c".to_string(), "3".to_string()]);

    server.shutdown().await;
}

#[tokio::test]
async fn zadd_flags_and_lex_ranges() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let _: i64 = redis::cmd("ZADD")
        .arg("zf")
        .arg(1.0)
        .arg("a")
        .query_async(&mut conn)
        .await
        .unwrap();

    // NX must not update an existing member.
    let nx_added: i64 = redis::cmd("ZADD")
        .arg("zf")
        .arg("NX")
        .arg(5.0)
        .arg("a")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(nx_added, 0);
    let s: f64 = redis::cmd("ZSCORE")
        .arg("zf")
        .arg("a")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(s, 1.0, "NX should not have changed the score");

    // GT + CH: 0.5 < 1 so no change; 10 > 1 so changed.
    let ch_no: i64 = redis::cmd("ZADD")
        .arg("zf")
        .arg("GT")
        .arg("CH")
        .arg(0.5)
        .arg("a")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(ch_no, 0);
    let ch_yes: i64 = redis::cmd("ZADD")
        .arg("zf")
        .arg("GT")
        .arg("CH")
        .arg(10.0)
        .arg("a")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(ch_yes, 1);

    // ZRANGEBYLEX over equal-scored members.
    let _: i64 = redis::cmd("ZADD")
        .arg("zl")
        .arg(0.0)
        .arg("a")
        .arg(0.0)
        .arg("b")
        .arg(0.0)
        .arg("c")
        .arg(0.0)
        .arg("d")
        .query_async(&mut conn)
        .await
        .unwrap();
    let lex: Vec<String> = redis::cmd("ZRANGEBYLEX")
        .arg("zl")
        .arg("[a")
        .arg("(c")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(lex, vec!["a", "b"], "[a (c includes a,b but excludes c");

    let lexcount: i64 = redis::cmd("ZLEXCOUNT")
        .arg("zl")
        .arg("-")
        .arg("+")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(lexcount, 4);

    server.shutdown().await;
}
