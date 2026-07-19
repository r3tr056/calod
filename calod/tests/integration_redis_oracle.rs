mod common;

use std::collections::HashMap;
use std::time::Duration;

use redis::{AsyncCommands, Client};
use uuid::Uuid;

async fn connect(url: &str) -> redis::aio::MultiplexedConnection {
    let client = Client::open(url).expect("create redis client");
    tokio::time::timeout(
        Duration::from_secs(5),
        client.get_multiplexed_async_connection(),
    )
    .await
    .expect("connect timeout")
    .expect("connect")
}

fn oracle_url() -> Option<String> {
    common::redis_oracle_url()
}

#[tokio::test]
async fn oracle_matches_basic_string_and_option_contracts() {
    let Some(redis_url) = oracle_url() else {
        return;
    };

    let server = common::TestServer::spawn().await;
    let mut calod = connect(&server.redis_url()).await;
    let mut redis = connect(&redis_url).await;

    let prefix = format!("oracle:{}:", Uuid::new_v4());
    let key = format!("{prefix}k");
    let missing = format!("{prefix}missing");

    let calod_first: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg("v1")
        .arg("NX")
        .query_async(&mut calod)
        .await
        .expect("calod set nx");
    let redis_first: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg("v1")
        .arg("NX")
        .query_async(&mut redis)
        .await
        .expect("redis set nx");
    assert_eq!(calod_first, redis_first);

    let calod_second: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg("v2")
        .arg("NX")
        .query_async(&mut calod)
        .await
        .expect("calod set nx no-op");
    let redis_second: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg("v2")
        .arg("NX")
        .query_async(&mut redis)
        .await
        .expect("redis set nx no-op");
    assert_eq!(calod_second, redis_second);

    let calod_missing_xx: Option<String> = redis::cmd("SET")
        .arg(&missing)
        .arg("v")
        .arg("XX")
        .query_async(&mut calod)
        .await
        .expect("calod set xx no-op");
    let redis_missing_xx: Option<String> = redis::cmd("SET")
        .arg(&missing)
        .arg("v")
        .arg("XX")
        .query_async(&mut redis)
        .await
        .expect("redis set xx no-op");
    assert_eq!(calod_missing_xx, redis_missing_xx);

    let calod_get: Option<String> = calod.get(&key).await.expect("calod get");
    let redis_get: Option<String> = redis.get(&key).await.expect("redis get");
    assert_eq!(calod_get, redis_get);

    let calod_missing_get: Option<String> = calod.get(&missing).await.expect("calod get missing");
    let redis_missing_get: Option<String> = redis.get(&missing).await.expect("redis get missing");
    assert_eq!(calod_missing_get, redis_missing_get);

    let calod_mget: Vec<Option<String>> = redis::cmd("MGET")
        .arg(&[key.as_str(), missing.as_str()])
        .query_async(&mut calod)
        .await
        .expect("calod mget");
    let redis_mget: Vec<Option<String>> = redis::cmd("MGET")
        .arg(&[key.as_str(), missing.as_str()])
        .query_async(&mut redis)
        .await
        .expect("redis mget");
    assert_eq!(calod_mget, redis_mget);

    server.shutdown().await;
}

#[tokio::test]
async fn oracle_matches_hash_and_scan_shapes() {
    let Some(redis_url) = oracle_url() else {
        return;
    };

    let server = common::TestServer::spawn().await;
    let mut calod = connect(&server.redis_url()).await;
    let mut redis = connect(&redis_url).await;

    let prefix = format!("oracle:{}:", Uuid::new_v4());
    let hash_key = format!("{prefix}hash");

    let _: usize = redis::cmd("HSET")
        .arg(&hash_key)
        .arg("f1")
        .arg("v1")
        .arg("f2")
        .arg("v2")
        .query_async(&mut calod)
        .await
        .expect("calod hset");
    let _: usize = redis::cmd("HSET")
        .arg(&hash_key)
        .arg("f1")
        .arg("v1")
        .arg("f2")
        .arg("v2")
        .query_async(&mut redis)
        .await
        .expect("redis hset");

    let calod_hgetall: HashMap<String, String> = redis::cmd("HGETALL")
        .arg(&hash_key)
        .query_async(&mut calod)
        .await
        .expect("calod hgetall");
    let redis_hgetall: HashMap<String, String> = redis::cmd("HGETALL")
        .arg(&hash_key)
        .query_async(&mut redis)
        .await
        .expect("redis hgetall");
    assert_eq!(calod_hgetall, redis_hgetall);

    for idx in 0..6 {
        let key = format!("{prefix}scan:{idx}");
        let _: () = calod.set(&key, idx).await.expect("calod set scan key");
        let _: () = redis.set(&key, idx).await.expect("redis set scan key");
    }

    let mut calod_scan: (u64, Vec<String>) = redis::cmd("SCAN")
        .arg(0)
        .arg("MATCH")
        .arg(format!("{prefix}scan:*"))
        .arg("COUNT")
        .arg(20)
        .query_async(&mut calod)
        .await
        .expect("calod scan");
    let mut redis_scan: (u64, Vec<String>) = redis::cmd("SCAN")
        .arg(0)
        .arg("MATCH")
        .arg(format!("{prefix}scan:*"))
        .arg("COUNT")
        .arg(20)
        .query_async(&mut redis)
        .await
        .expect("redis scan");

    calod_scan.1.sort();
    redis_scan.1.sort();
    assert_eq!(calod_scan, redis_scan);

    server.shutdown().await;
}
