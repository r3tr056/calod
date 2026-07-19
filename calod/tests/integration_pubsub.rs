//! End-to-end pub/sub over the real wire: a redis-rs subscriber receives a
//! message published by a separate connection, exercising the per-connection
//! push channel and the `PubSubRegistry` fan-out.

mod common;

use std::time::Duration;

use common::TestServer;
use futures_util::StreamExt;

#[tokio::test]
async fn publish_reaches_channel_subscriber() {
    let server = TestServer::spawn().await;
    let url = server.redis_url();

    // Subscriber on its own connection.
    let sub_client = redis::Client::open(url.clone()).expect("sub client");
    let mut pubsub = sub_client
        .get_async_pubsub()
        .await
        .expect("pubsub connection");
    pubsub.subscribe("news").await.expect("subscribe news");

    // Publisher on a separate connection. Retry until the subscription is
    // visible (PUBLISH reports >=1 receiver), since subscribe registration and
    // the publish race across two connections.
    let pub_client = redis::Client::open(url).expect("pub client");
    let mut pubcon = pub_client
        .get_multiplexed_async_connection()
        .await
        .expect("publisher connection");

    let mut delivered = 0i64;
    for _ in 0..50 {
        delivered = redis::cmd("PUBLISH")
            .arg("news")
            .arg("hello")
            .query_async(&mut pubcon)
            .await
            .expect("publish");
        if delivered >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        delivered >= 1,
        "PUBLISH should report at least one receiver"
    );

    // The subscriber must receive the message.
    let mut stream = pubsub.on_message();
    let msg = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("message should arrive within timeout")
        .expect("stream should yield a message");
    let channel = msg.get_channel_name().to_string();
    let payload: String = msg.get_payload().expect("payload is a utf8 string");
    assert_eq!(channel, "news");
    assert_eq!(payload, "hello");

    drop(stream);
    drop(pubsub);
    server.shutdown().await;
}

#[tokio::test]
async fn pattern_subscriber_receives_pmessage() {
    let server = TestServer::spawn().await;
    let url = server.redis_url();

    let sub_client = redis::Client::open(url.clone()).expect("sub client");
    let mut pubsub = sub_client
        .get_async_pubsub()
        .await
        .expect("pubsub connection");
    pubsub.psubscribe("news.*").await.expect("psubscribe");

    let pub_client = redis::Client::open(url).expect("pub client");
    let mut pubcon = pub_client
        .get_multiplexed_async_connection()
        .await
        .expect("publisher connection");

    let mut delivered = 0i64;
    for _ in 0..50 {
        delivered = redis::cmd("PUBLISH")
            .arg("news.tech")
            .arg("payload")
            .query_async(&mut pubcon)
            .await
            .expect("publish");
        if delivered >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(delivered >= 1, "pattern subscriber should be a receiver");

    let mut stream = pubsub.on_message();
    let msg = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("pmessage should arrive within timeout")
        .expect("stream should yield a message");
    // For pmessage, the matched channel is the concrete channel name.
    assert_eq!(msg.get_channel_name(), "news.tech");
    let payload: String = msg.get_payload().expect("payload");
    assert_eq!(payload, "payload");

    drop(stream);
    drop(pubsub);
    server.shutdown().await;
}
