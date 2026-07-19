//! End-to-end Lua scripting (Phase 10) via redis-rs: EVAL / EVALSHA / SCRIPT
//! and the `redis.call` re-entrancy bridge.

mod common;

use common::TestServer;

#[tokio::test]
async fn eval_basic_return_types() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let n: i64 = redis::cmd("EVAL")
        .arg("return 1 + 2")
        .arg(0)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(n, 3);

    let s: String = redis::cmd("EVAL")
        .arg("return 'hello'")
        .arg(0)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(s, "hello");

    let arr: Vec<i64> = redis::cmd("EVAL")
        .arg("return {1, 2, 3}")
        .arg(0)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(arr, vec![1, 2, 3]);

    // Status reply -> redis-rs sees "OK".
    let ok: String = redis::cmd("EVAL")
        .arg("return redis.status_reply('OK')")
        .arg(0)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(ok, "OK");

    // KEYS / ARGV are passed through.
    let echoed: Vec<String> = redis::cmd("EVAL")
        .arg("return {KEYS[1], ARGV[1]}")
        .arg(1)
        .arg("mykey")
        .arg("myarg")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(echoed, vec!["mykey".to_string(), "myarg".to_string()]);

    // error_reply propagates as a server error.
    let err: redis::RedisResult<String> = redis::cmd("EVAL")
        .arg("return redis.error_reply('custom failure')")
        .arg(0)
        .query_async(&mut conn)
        .await;
    assert!(err.is_err());

    server.shutdown().await;
}

#[tokio::test]
async fn eval_redis_call_roundtrip() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    // A script that writes then reads back through redis.call.
    let set_ret: String = redis::cmd("EVAL")
        .arg("return redis.call('SET', KEYS[1], ARGV[1])")
        .arg(1)
        .arg("sk")
        .arg("sv")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(set_ret, "OK");

    // The write is visible outside the script.
    let got: String = redis::cmd("GET").arg("sk").query_async(&mut conn).await.unwrap();
    assert_eq!(got, "sv");

    // redis.call GET inside the script returns the value.
    let in_script: String = redis::cmd("EVAL")
        .arg("return redis.call('GET', KEYS[1])")
        .arg(1)
        .arg("sk")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(in_script, "sv");

    // pcall lets a script trap a WRONGTYPE without aborting.
    let _: i64 = redis::cmd("RPUSH")
        .arg("listk")
        .arg("a")
        .query_async(&mut conn)
        .await
        .unwrap();
    let trapped: String = redis::cmd("EVAL")
        .arg("local r = redis.pcall('GET', KEYS[1]); if r.err then return 'trapped' else return r end")
        .arg(1)
        .arg("listk")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(trapped, "trapped");

    // INCR via redis.call returns an integer.
    let counter: i64 = redis::cmd("EVAL")
        .arg("return redis.call('INCR', KEYS[1])")
        .arg(1)
        .arg("cnt")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(counter, 1);

    server.shutdown().await;
}

#[tokio::test]
async fn script_load_evalsha_exists() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let body = "return redis.call('SET', KEYS[1], ARGV[1])";

    let sha: String = redis::cmd("SCRIPT")
        .arg("LOAD")
        .arg(body)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(sha.len(), 40);

    // SCRIPT EXISTS reports the loaded digest plus a missing one.
    let exists: Vec<i64> = redis::cmd("SCRIPT")
        .arg("EXISTS")
        .arg(&sha)
        .arg("0000000000000000000000000000000000000000")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(exists, vec![1, 0]);

    // EVALSHA runs the cached script.
    let ok: String = redis::cmd("EVALSHA")
        .arg(&sha)
        .arg(1)
        .arg("ek")
        .arg("ev")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(ok, "OK");
    let got: String = redis::cmd("GET").arg("ek").query_async(&mut conn).await.unwrap();
    assert_eq!(got, "ev");

    // Unknown SHA -> NOSCRIPT error.
    let missing: redis::RedisResult<String> = redis::cmd("EVALSHA")
        .arg("0000000000000000000000000000000000000000")
        .arg(0)
        .query_async(&mut conn)
        .await;
    assert!(missing.is_err());

    server.shutdown().await;
}

#[tokio::test]
async fn redis_rs_script_helper() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    // The high-level Script API: it tries EVALSHA, falls back to EVAL on
    // NOSCRIPT, then can reuse the cached SHA — exercising the full flow.
    let script = redis::Script::new("return redis.call('SET', KEYS[1], ARGV[1])");
    let _: String = script
        .key("hk")
        .arg("hv")
        .invoke_async(&mut conn)
        .await
        .unwrap();
    let got: String = redis::cmd("GET").arg("hk").query_async(&mut conn).await.unwrap();
    assert_eq!(got, "hv");

    // Invoke again (now the SHA is cached server-side -> EVALSHA path).
    let _: String = script
        .key("hk2")
        .arg("hv2")
        .invoke_async(&mut conn)
        .await
        .unwrap();
    let got2: String = redis::cmd("GET").arg("hk2").query_async(&mut conn).await.unwrap();
    assert_eq!(got2, "hv2");

    server.shutdown().await;
}
