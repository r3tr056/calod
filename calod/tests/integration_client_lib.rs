use std::net::SocketAddr;
use std::time::Duration;

use redis::{AsyncCommands, Client};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

async fn spawn_server(
    listener: TcpListener,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let store =
            std::sync::Arc::new(calod::cld_srv::served_store::ServedCalodStore::new().await);
        let active = std::sync::Arc::new(dashmap::DashMap::with_hasher(ahash::RandomState::new()));

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    break;
                }
                accept = listener.accept() => {
                    let Ok((stream, peer)) = accept else { continue; };
                    let store = store.clone();
                    let active = active.clone();

                    tokio::spawn(async move {
                        let metrics = std::sync::Arc::new(calod::cld_srv::metrics::MetricsAggregator::new());
                        let auth = std::sync::Arc::new(calod::cld_srv::auth::AuthManager::new(false));
                        let conn = calod::cld_srv::connection::Connection::new(
                            store.clone(),
                            calod::cld_srv::connection::ConnectionConfig::default(),
                            metrics,
                            auth,
                        );
                        let conn_arc = std::sync::Arc::new(tokio::sync::Mutex::new(conn));
                        let id = uuid::Uuid::new_v4();
                        let connection = {
                            let mut guard = conn_arc.lock().await;
                            guard.set_client_id(id);
                            guard.set_peer_address(peer);
                            guard.clone()
                        };
                        active.insert(id, std::sync::Arc::downgrade(&conn_arc));

                        let process_fut = async {
                            let _ = connection.process(stream, active.clone(), conn_arc.clone(), None).await;
                        };

                        let _ = tokio::time::timeout(Duration::from_secs(2), process_fut).await;
                        active.remove(&id);
                    });
                }
            }
        }
    })
}

#[tokio::test]
async fn raw_resp_socket_ping_works() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = spawn_server(listener, shutdown_rx).await;

    let mut stream =
        tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr))
            .await
            .expect("connect timeout")
            .expect("connect");

    stream
        .write_all(b"*1\r\n$4\r\nPING\r\n")
        .await
        .expect("write ping");

    let mut buf = [0_u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .expect("read timeout")
        .expect("read");
    assert!(n > 0);
    let resp = String::from_utf8_lossy(&buf[..n]);
    assert!(resp.contains("+PONG"));

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn redis_async_client_can_set_get_ping() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr: SocketAddr = listener.local_addr().expect("local addr");

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server = spawn_server(listener, shutdown_rx).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let client =
        Client::open(format!("redis://{}/?protocol=resp2", addr)).expect("create redis client");
    let mut conn = tokio::time::timeout(
        Duration::from_secs(5),
        client.get_multiplexed_async_connection(),
    )
    .await
    .expect("connect timeout")
    .expect("connect redis async client");

    let pong: String = tokio::time::timeout(
        Duration::from_secs(5),
        redis::cmd("PING").query_async(&mut conn),
    )
    .await
    .expect("PING timeout")
    .expect("PING works");
    assert_eq!(pong, "PONG");

    let _: () = tokio::time::timeout(Duration::from_secs(5), conn.set("lib:key", "lib:value"))
        .await
        .expect("SET timeout")
        .expect("SET works");
    let v: String = tokio::time::timeout(Duration::from_secs(5), conn.get("lib:key"))
        .await
        .expect("GET timeout")
        .expect("GET works");
    assert_eq!(v, "lib:value");

    drop(conn);
    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn redis_async_client_observes_set_nx_xx_nil_contracts() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr: SocketAddr = listener.local_addr().expect("local addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = spawn_server(listener, shutdown_rx).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let client =
        Client::open(format!("redis://{}/?protocol=resp2", addr)).expect("create redis client");
    let mut conn = tokio::time::timeout(
        Duration::from_secs(5),
        client.get_multiplexed_async_connection(),
    )
    .await
    .expect("connect timeout")
    .expect("connect redis async client");

    let first: Option<String> = redis::cmd("SET")
        .arg("opt:key")
        .arg("v1")
        .arg("NX")
        .query_async(&mut conn)
        .await
        .expect("SET NX should succeed");
    assert_eq!(first.as_deref(), Some("OK"));

    let second: Option<String> = redis::cmd("SET")
        .arg("opt:key")
        .arg("v2")
        .arg("NX")
        .query_async(&mut conn)
        .await
        .expect("SET NX no-op should be valid");
    assert_eq!(second, None);

    let missing_xx: Option<String> = redis::cmd("SET")
        .arg("missing:xx")
        .arg("v")
        .arg("XX")
        .query_async(&mut conn)
        .await
        .expect("SET XX no-op should be valid");
    assert_eq!(missing_xx, None);

    let existing_xx: Option<String> = redis::cmd("SET")
        .arg("opt:key")
        .arg("v3")
        .arg("XX")
        .query_async(&mut conn)
        .await
        .expect("SET XX should succeed");
    assert_eq!(existing_xx.as_deref(), Some("OK"));

    let final_value: String = conn.get("opt:key").await.expect("GET final value");
    assert_eq!(final_value, "v3");

    drop(conn);
    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}
