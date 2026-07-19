//! Phase 1 handshake-completeness coverage via a real redis-rs client:
//! CONFIG GET/SET, CLIENT ID, COMMAND COUNT/DOCS, and enriched INFO. These are
//! the surfaces mainstream clients probe on connect, so they must answer rather
//! than error.

mod common;

use std::collections::HashMap;

use common::TestServer;

#[tokio::test]
async fn config_get_set_roundtrips_and_globs() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    // GET a known param: must return a value, never an error.
    let m: HashMap<String, String> = redis::cmd("CONFIG")
        .arg("GET")
        .arg("maxmemory")
        .query_async(&mut conn)
        .await
        .expect("CONFIG GET maxmemory");
    assert!(m.contains_key("maxmemory"));

    // SET then GET reflects the override.
    let _: () = redis::cmd("CONFIG")
        .arg("SET")
        .arg("maxmemory-policy")
        .arg("noeviction")
        .query_async(&mut conn)
        .await
        .expect("CONFIG SET");
    let m2: HashMap<String, String> = redis::cmd("CONFIG")
        .arg("GET")
        .arg("maxmemory-policy")
        .query_async(&mut conn)
        .await
        .expect("CONFIG GET maxmemory-policy");
    assert_eq!(
        m2.get("maxmemory-policy").map(String::as_str),
        Some("noeviction")
    );

    // Glob pattern returns multiple params.
    let mg: HashMap<String, String> = redis::cmd("CONFIG")
        .arg("GET")
        .arg("max*")
        .query_async(&mut conn)
        .await
        .expect("CONFIG GET max*");
    assert!(mg.len() >= 2, "glob should match several max* params");

    // Unknown param GET is an empty map, not an error.
    let mu: HashMap<String, String> = redis::cmd("CONFIG")
        .arg("GET")
        .arg("totally-not-a-real-param")
        .query_async(&mut conn)
        .await
        .expect("CONFIG GET unknown must not error");
    assert!(mu.is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn config_get_works_over_resp3_map() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url_resp3()).expect("resp3 client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("resp3 connect");

    let m: HashMap<String, String> = redis::cmd("CONFIG")
        .arg("GET")
        .arg("maxmemory")
        .query_async(&mut conn)
        .await
        .expect("CONFIG GET over RESP3");
    assert!(m.contains_key("maxmemory"));

    server.shutdown().await;
}

#[tokio::test]
async fn client_id_command_count_and_docs() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let id: i64 = redis::cmd("CLIENT")
        .arg("ID")
        .query_async(&mut conn)
        .await
        .expect("CLIENT ID");
    assert!(id >= 0, "CLIENT ID must be a non-negative integer");

    let count: i64 = redis::cmd("COMMAND")
        .arg("COUNT")
        .query_async(&mut conn)
        .await
        .expect("COMMAND COUNT");
    assert!(count > 0);

    // COMMAND DOCS must succeed (an empty reply is acceptable).
    let _docs: redis::Value = redis::cmd("COMMAND")
        .arg("DOCS")
        .query_async(&mut conn)
        .await
        .expect("COMMAND DOCS must not error");

    server.shutdown().await;
}

#[tokio::test]
async fn info_advertises_redis_version_and_run_id() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    let info: String = redis::cmd("INFO")
        .query_async(&mut conn)
        .await
        .expect("INFO");
    assert!(
        info.contains("redis_version:7.4.0"),
        "INFO must advertise a redis_version clients can gate on"
    );
    assert!(info.contains("run_id:"));
    assert!(info.contains("maxmemory:"));

    server.shutdown().await;
}

#[tokio::test]
async fn info_section_filtering_returns_only_requested_section() {
    let server = TestServer::spawn().await;
    let client = redis::Client::open(server.redis_url()).expect("client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect");

    // INFO server must return ONLY the Server section, not all sections.
    let server_info: String = redis::cmd("INFO")
        .arg("server")
        .query_async(&mut conn)
        .await
        .expect("INFO server");
    assert!(server_info.contains("# Server"));
    assert!(server_info.contains("redis_version:7.4.0"));
    assert!(
        !server_info.contains("# Memory"),
        "INFO server must not include the Memory section"
    );
    assert!(
        !server_info.contains("# Replication"),
        "INFO server must not include the Replication section"
    );
    // The old non-standard '# Section:' header must be gone.
    assert!(!server_info.contains("# Section:"));

    // INFO memory must return ONLY the Memory section.
    let mem_info: String = redis::cmd("INFO")
        .arg("memory")
        .query_async(&mut conn)
        .await
        .expect("INFO memory");
    assert!(mem_info.contains("# Memory"));
    assert!(mem_info.contains("maxmemory:"));
    assert!(!mem_info.contains("# Server"));

    server.shutdown().await;
}
