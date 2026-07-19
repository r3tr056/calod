#![allow(dead_code)]

use std::sync::{Arc, Weak};

use ahash::RandomState;
use calod::cld_srv::auth::AuthManager;
use calod::cld_srv::command::Command;
use calod::cld_srv::connection::{Connection, ConnectionConfig};
use calod::cld_srv::metrics::MetricsAggregator;
use calod::cld_srv::served_store::ServedCalodStore;
use calod::parser::parser::RESPParser;
use calod::parser::value::Value;
use dashmap::DashMap;
use redis::Client;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use uuid::Uuid;

type ClientId = Uuid;

pub struct Harness {
    pub store: Arc<ServedCalodStore>,
    pub active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
    pub connection: Arc<Mutex<Connection>>,
}

pub struct TestServer {
    pub addr: std::net::SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl Harness {
    pub async fn new() -> Self {
        let store = Arc::new(ServedCalodStore::new().await);
        let active_connections = Arc::new(DashMap::with_hasher(ahash::RandomState::new()));
        let metrics = Arc::new(MetricsAggregator::new());
        let auth = Arc::new(AuthManager::new(false));
        let connection = Arc::new(Mutex::new(Connection::new(
            store.clone(),
            ConnectionConfig::default(),
            metrics,
            auth,
        )));

        Self {
            store,
            active_connections,
            connection,
        }
    }

    pub async fn exec_raw(&self, cmd: Command) -> Vec<u8> {
        // Read the live protocol context from the connection so a `HELLO 3`
        // issued through this harness switches subsequent replies to RESP3,
        // mirroring the production writer task.
        let ctx = self.connection.lock().await.ctx();
        self.store
            .execute(
                cmd,
                self.active_connections.clone(),
                self.connection.clone(),
                ctx,
            )
            .await
    }

    pub async fn exec(&self, cmd: Command) -> Value {
        let raw = self.exec_raw(cmd).await;
        let (value, consumed) = RESPParser::parse_value(&raw).expect("response must be valid RESP");
        assert_eq!(consumed, raw.len(), "response contains trailing bytes");
        value
    }
}

impl TestServer {
    pub async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener address");
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let join = tokio::spawn(async move {
            let store = std::sync::Arc::new(ServedCalodStore::new().await);
            let active = std::sync::Arc::new(DashMap::with_hasher(ahash::RandomState::new()));

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accept = listener.accept() => {
                        let Ok((stream, peer)) = accept else { continue; };
                        let store = store.clone();
                        let active = active.clone();

                        tokio::spawn(async move {
                            let metrics = std::sync::Arc::new(MetricsAggregator::new());
                            let auth = std::sync::Arc::new(AuthManager::new(false));
                            let conn = Connection::new(
                                store.clone(),
                                ConnectionConfig::default(),
                                metrics,
                                auth,
                            );
                            let conn_arc = std::sync::Arc::new(Mutex::new(conn));
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

                            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), process_fut).await;
                            active.remove(&id);
                        });
                    }
                }
            }
        });

        Self {
            addr,
            shutdown_tx: Some(shutdown_tx),
            join: Some(join),
        }
    }

    pub fn redis_url(&self) -> String {
        format!("redis://{}/?protocol=resp2", self.addr)
    }

    pub fn redis_url_resp3(&self) -> String {
        format!("redis://{}/?protocol=resp3", self.addr)
    }

    pub fn redis_client(&self) -> Client {
        Client::open(self.redis_url()).expect("create redis client")
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), join).await;
        }
    }
}

#[allow(dead_code)]
pub fn bulk_to_string(value: &Value) -> Option<String> {
    if let Value::BulkString(bytes) = value {
        String::from_utf8(bytes.clone()).ok()
    } else {
        None
    }
}

pub fn redis_oracle_url() -> Option<String> {
    std::env::var("REDIS_ORACLE_URL")
        .ok()
        .filter(|s| !s.is_empty())
}
