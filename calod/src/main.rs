
extern crate core;

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[allow(unused_imports)]
use std::env;

#[allow(unused_imports)]
use std::fs;

#[allow(unused_imports)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use calod::request_response::pool::create_connection_pool;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let pool = create_connection_pool().await?;
    let listener = TcpListener::bind("127.0.0.1:6379").await?;

    tracing::info!("Listening on 127.0.0.1:6379");

    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                tracing::debug!("Accepted connection from {}", addr);
                let pool = pool.clone();
                tokio::spawn(async move {
                    let mut conn = match pool.get().await {
                        Ok(conn) => {
                            tracing::debug!("Connection from pool acquired for {}", addr);
                            conn
                        },
                        Err(e) => {
                            tracing::debug!("Failed to get connection from pool for {}:{}", addr, e);
                            return; // Exit the task if conn pool fails.
                        },
                    };
                    conn.process(socket).await;
                    tracing::debug!("Connection processing finished for {}", addr);
                });
            },
            Err(e) => {
                tracing::error!("Error accepting connection: {}", e)
            },
        }
    }
}