//! End-to-end keyspace notifications: enabling `notify-keyspace-events` makes
//! mutations publish to `__keyspace@0__` / `__keyevent@0__` channels.

mod common;

use std::time::Duration;

use common::TestServer;
use futures_util::StreamExt;

#[tokio::test]
async fn keyevent_and_keyspace_fire_on_mutation() {
    let server = TestServer::spawn().await;
    let url = server.redis_url();

    // Enable keyspace events (shared config across the server's connections).
    let cfg_client = redis::Client::open(url.clone()).expect("cfg client");
    let mut cfg = cfg_client
        .get_multiplexed_async_connection()
        .await
        .expect("cfg connect");
    let _: () = redis::cmd("CONFIG")
        .arg("SET")
        .arg("notify-keyspace-events")
        .arg("KEA")
        .query_async(&mut cfg)
        .await
        .expect("config set");

    // Subscribe to both the keyevent and keyspace channels for a SET on "foo".
    let sub_client = redis::Client::open(url.clone()).expect("sub client");
    let mut pubsub = sub_client.get_async_pubsub().await.expect("pubsub");
    pubsub
        .subscribe("__keyevent@0__:set")
        .await
        .expect("sub keyevent");
    pubsub
        .subscribe("__keyspace@0__:foo")
        .await
        .expect("sub keyspace");

    // Mutate from a separate connection.
    let w_client = redis::Client::open(url).expect("writer client");
    let mut w = w_client
        .get_multiplexed_async_connection()
        .await
        .expect("writer connect");
    let _: () = redis::cmd("SET")
        .arg("foo")
        .arg("bar")
        .query_async(&mut w)
        .await
        .unwrap();

    // Collect the two notifications (order between them is not guaranteed).
    let mut stream = pubsub.on_message();
    let mut got_keyevent = false;
    let mut got_keyspace = false;
    for _ in 0..2 {
        let msg = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .expect("notification within timeout")
            .expect("a message");
        let channel = msg.get_channel_name().to_string();
        let payload: String = msg.get_payload().unwrap();
        match channel.as_str() {
            "__keyevent@0__:set" => {
                assert_eq!(payload, "foo");
                got_keyevent = true;
            }
            "__keyspace@0__:foo" => {
                assert_eq!(payload, "set");
                got_keyspace = true;
            }
            other => panic!("unexpected channel {other}"),
        }
    }
    assert!(got_keyevent && got_keyspace);

    server.shutdown().await;
}
