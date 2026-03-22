/*
 * CALOD Server - Cache a Lot of Data (Server Edition)
 * Copyright (c) 2025 CALOD.
 *
 * Licensed under the MIT License.
 *
 * Author: Ankur Debnath
 * Company: CALOD
 * Product: CALOD Server
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

extern crate core;

use uuid::Uuid;
use clap::Parser;

use ahash::RandomState;
use mimalloc::MiMalloc;

use dashmap::DashMap;

use tokio::sync::Mutex;
use tokio::signal;
use tokio::net::TcpSocket;
use tokio::task::JoinSet;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc,Weak};
use std::time::Duration;

use calod::cld_srv::connection::Connection;
use calod::cld_srv::pool::{create_connection_pool, create_secure_connection_pool};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

type ClientId = Uuid;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(version=VERSION, about="CALOD server")]
struct Cli {
    /// Bind address (IPv4 and IPv6)
    #[clap(long, default_value="0.0.0.0")]
    address: String,

    /// Port to listen on
    #[clap(long, default_value = "6379")]
    port: u16,
    
    /// Authentication password (optional)
    #[clap(long)]
    password: Option<String>,
    
    /// Maximum number of connections
    #[clap(long, default_value = "100000")]
    max_connections: usize,
    
    /// TCP backlog size
    #[clap(long, default_value = "1024")]
    tcp_backlog: u32,
    
    /// TCP keep-alive in seconds (0 to disable)
    #[clap(long, default_value = "300")]
    tcp_keepalive: u64,

    /// Enable SO_REUSEADDR for TCP sockets
    #[clap(long)]
    reuse_address: bool,

}

fn print_banner(host: &str, port: u16, auth_enabled: bool) {
    println!("
 ██████╗ █████╗ ██╗      ██████╗ ██████╗ 
██╔════╝██╔══██╗██║     ██╔═══██╗██╔══██╗
██║     ███████║██║     ██║   ██║██║  ██║
██║     ██╔══██║██║     ██║   ██║██║  ██║
╚██████╗██║  ██║███████╗╚██████╔╝██████╔╝
 ╚═════╝╚═╝  ╚═╝╚══════╝ ╚═════╝ ╚═════╝                        
    ");
    println!("┌───────────────────────────────────────┐");
    println!("│ Calod Cache Server v{}             │", VERSION);
    println!("│ High performance Redis-compatible    │");
    println!("│ in-memory datastore                  │");
    println!("├───────────────────────────────────────┤");
    println!("│ Listening on: {}:{:<14}│", host, port);
    println!("│ Authentication: {:<20}│", if auth_enabled { "Enabled" } else { "Disabled" });
    println!("│ Date: 2025-03-11                     │");
    println!("└───────────────────────────────────────┘");
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args = Cli::parse();

    // resolve listen address
    let addr = IpAddr::from_str(&args.address).unwrap_or(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
    let socket_addr = SocketAddr::new(addr, args.port);

    let auth_required = args.password.is_some();

    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };

    socket.set_reuseaddr(args.reuse_address)?;

    socket.bind(socket_addr)?;
    let listener = socket.listen(args.tcp_backlog)?;

    print_banner(&args.address, args.port, auth_required);

    tracing::info!("Server started on {}:{}", args.address, args.port);

    let pool = if let Some(password) = args.password {
        create_secure_connection_pool(Some(&password)).await?
    } else {
        create_connection_pool().await?
    };

    // Global connection tracking
    let active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>> = 
        Arc::new(DashMap::with_capacity_and_hasher(1024, ahash::RandomState::new()));

    // Create channel for shutdown signal
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel(1);

    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        match signal::ctrl_c().await {
            Ok(()) => {
                tracing::info!("Shutdown signal received, initiating graceful shutdown");
                let _ = shutdown_tx_clone.send(());
            }
            Err(err) => {
                tracing::error!("Unable to listen for shutdown signal: {}", err);
            }
        }
    });

    let server_task = tokio::spawn(async move {
        let mut handles = JoinSet::new();

        loop {
            // Accept new connections or handle shutdown
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, addr)) => {
                            tracing::debug!("Accepted connection from {}", addr);
                            
                            // Configure socket for performance
                            let socket_ref = socket2::SockRef::from(&stream);
                            let mut ka = socket2::TcpKeepalive::new();
                            ka = ka.with_time(Duration::from_secs(args.tcp_keepalive));
                            ka = ka.with_interval(Duration::from_secs(args.tcp_keepalive));
                            socket_ref.set_tcp_keepalive(&ka).unwrap();
                            
                            if let Err(e) = stream.set_nodelay(true) {
                                tracing::warn!("Failed to set TCP_NODELAY: {}", e);
                            }

                            let pool_clone = pool.clone();
                            let active_connections_clone = active_connections.clone();

                            handles.spawn(async move {
                                let client_id = Uuid::new_v4();
                                
                                if let Ok(conn) = pool_clone.get().await {
                                    let conn_arc: Arc<Mutex<Connection>> = Arc::new(Mutex::new(conn.lock().await.clone())); 
                                    {
                                        let mut connection = conn_arc.lock().await;
                                        connection.set_client_id(client_id);
                                        connection.set_peer_address(addr);
                                    }
                                    
                                    // Register the connection
                                    active_connections_clone.insert(client_id, Arc::downgrade(&conn_arc));
                                    pool_clone.register_connection(conn_arc.clone());

                                    // Process the connection
                                    let mut connection = conn_arc.lock().await;
                                    connection.process(stream, active_connections_clone.clone(), Some(&pool_clone.connection_semaphore())).await;

                                    // Clean up the connection
                                    active_connections_clone.remove(&client_id);
                                    pool_clone.unregister_connection(client_id);
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
                },
                _ = shutdown_rx.recv() => {
                    tracing::info!("Shutting down server");
                    break;
                }
            }
        }

        let cleanup_timeout = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(join_result) = handles.join_next().await {
                if let Err(e) = join_result {
                    tracing::error!("Join error: {}", e);
                }
            }
        });

        match cleanup_timeout.await {
            Ok(_) => tracing::info!("All connections cleaned up successfully"),
            Err(_) => tracing::warn!("Timed out waiting for some connections to close"),
        }
    });

    server_task.await?;

    tracing::info!("Server shutdown complete");
    Ok(())
}