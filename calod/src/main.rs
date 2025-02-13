
extern crate core;

use calod::request_response::server::Connection;
use dashmap::DashMap;
use mimalloc::MiMalloc;
use tokio::sync::Mutex;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;


#[allow(unused_imports)]
use std::env;

#[allow(unused_imports)]
use std::fs;
use std::sync::Arc;
use std::sync::Weak;

#[allow(unused_imports)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use calod::request_response::pool::create_connection_pool;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_banner() {
    println!("
 ██████╗ █████╗ ██╗      ██████╗ ██████╗ 
██╔════╝██╔══██╗██║     ██╔═══██╗██╔══██╗
██║     ███████║██║     ██║   ██║██║  ██║
██║     ██╔══██║██║     ██║   ██║██║  ██║
╚██████╗██║  ██║███████╗╚██████╔╝██████╔╝
 ╚═════╝╚═╝  ╚═╝╚══════╝ ╚═════╝ ╚═════╝                        
    ");
    println!("Calod Cache Server v{}", VERSION);
    println!("Listening on 127.0.0.1:6379");
    println!("-----------------------------------");
}

type ClientId = Uuid;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    print_banner();

    let pool = create_connection_pool().await?;
    let listener = TcpListener::bind("127.0.0.1:6379").await?;

    tracing::info!("Listening on 127.0.0.1:6379");
    
    let active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>>> = Arc::new(DashMap::new());

    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                tracing::debug!("Accepted connection from {}", addr);

                let pool_clone = pool.clone();
                let active_connections_clone = active_connections.clone();

                tokio::spawn(async move {
                    let client_id = Uuid::new_v4();
                    
                    if let Ok(conn) = pool_clone.get().await {
                        let conn_arc: Arc<Mutex<Connection>> = Arc::new(Mutex::new(conn.lock().await.clone())); 
                        let mut connection = conn_arc.lock().await;
                        connection.set_client_id(client_id);
                        
                        active_connections_clone.insert(client_id, Arc::downgrade(&conn_arc));

                        connection.process(socket, active_connections_clone.clone()).await;

                        active_connections_clone.remove(&client_id);
                        tracing::debug!("Connection cleaned up for {}", addr);
                    } else {
                        tracing::error!("Failed to get connection from pool for {}", addr);
                    }
                });
            },
            Err(e) => {
                tracing::error!("Error accepting connection: {}", e)
            },
        }
    }
}