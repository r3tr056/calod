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

use clap::Parser;
use uuid::Uuid;

use ahash::RandomState;
use mimalloc::MiMalloc;

use dashmap::DashMap;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpSocket};
use tokio::signal;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use calod::cld_srv::connection::Connection;
use calod::cld_srv::pool::{create_pool_from_config, Pool};
use calod::config::{resolve_config, CliOverrides};
use calod::tls::build_tls_acceptor;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

type ClientId = Uuid;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(version=VERSION, about="CALOD server")]
struct Cli {
    /// Path to a TOML config file (default: ./calod.toml if present)
    #[clap(long)]
    config: Option<PathBuf>,

    /// Bind address (IPv4 and IPv6) [default: 127.0.0.1]
    #[clap(long)]
    address: Option<String>,

    /// Port to listen on [default: 6379]
    #[clap(long)]
    port: Option<u16>,

    /// Authentication password (optional)
    #[clap(long)]
    password: Option<String>,

    /// Maximum number of connections
    #[clap(long)]
    max_connections: Option<usize>,

    /// TCP backlog size
    #[clap(long)]
    tcp_backlog: Option<u32>,

    /// TCP keep-alive in seconds (0 to disable)
    #[clap(long)]
    tcp_keepalive: Option<u64>,

    /// Enable SO_REUSEADDR for TCP sockets
    #[clap(long)]
    reuse_address: bool,

    /// Start with an EMPTY store if snapshot/AOF recovery fails (data loss)
    #[clap(long)]
    allow_data_loss: bool,

    /// Create an API key for the REST ledger API, print its one-time
    /// secret, and exit (does not start the server). Run this once
    /// against a stopped server (or before first start) to provision
    /// the first key; the server must be restarted afterwards if it
    /// was already running with an empty key store.
    #[clap(long)]
    create_api_key: bool,

    /// Tenant for --create-api-key [default: default]
    #[clap(long)]
    tenant: Option<String>,

    /// Project for --create-api-key [default: default]
    #[clap(long)]
    project: Option<String>,

    /// Comma-separated scopes for --create-api-key
    /// [default: effects:read,effects:write,effects:reconcile]
    #[clap(long)]
    scopes: Option<String>,
}

fn print_banner(host: &str, port: u16, auth_enabled: bool, tls_enabled: bool) {
    println!(
        "
 ██████╗ █████╗ ██╗      ██████╗ ██████╗ 
██╔════╝██╔══██╗██║     ██╔═══██╗██╔══██╗
██║     ███████║██║     ██║   ██║██║  ██║
██║     ██╔══██║██║     ██║   ██║██║  ██║
╚██████╗██║  ██║███████╗╚██████╔╝██████╔╝
 ╚═════╝╚═╝  ╚═╝╚══════╝ ╚═════╝ ╚═════╝                        
    "
    );
    println!("┌───────────────────────────────────────┐");
    println!("│ Calod Cache Server v{}             │", VERSION);
    println!("│ High performance Redis-compatible    │");
    println!("│ in-memory datastore                  │");
    println!("├───────────────────────────────────────┤");
    println!("│ Listening on: {}:{:<14}│", host, port);
    println!(
        "│ Authentication: {:<20}│",
        if auth_enabled { "Enabled" } else { "Disabled" }
    );
    println!(
        "│ TLS: {:<31}│",
        if tls_enabled { "Enabled" } else { "Disabled" }
    );
    println!("│ Date: 2025-03-11                     │");
    println!("└───────────────────────────────────────┘");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args = Cli::parse();

    // Resolve the single effective configuration:
    // CLI flags > CALOD_* env vars > TOML file > built-in defaults.
    let overrides = CliOverrides {
        config_path: args.config.clone(),
        address: args.address.clone(),
        port: args.port,
        password: args.password.clone(),
        max_connections: args.max_connections,
        tcp_backlog: args.tcp_backlog,
        tcp_keepalive: args.tcp_keepalive,
        reuse_address: args.reuse_address,
        allow_data_loss: args.allow_data_loss,
    };
    let resolved = resolve_config(&overrides, &|name| std::env::var(name).ok())?;
    let config = resolved.config;

    // One-shot provisioning: create a REST API key in the store at the
    // configured snapshot directory, print the one-time secret, exit.
    if args.create_api_key {
        use calod::store::api_keys::ApiKeyStore;
        use std::collections::BTreeSet;

        let tenant = args.tenant.as_deref().unwrap_or("default");
        let project = args.project.as_deref().unwrap_or("default");
        let scopes: BTreeSet<String> = args
            .scopes
            .as_deref()
            .unwrap_or("effects:read,effects:write,effects:reconcile")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        std::fs::create_dir_all(&config.persistence.snapshot_dir)?;
        let store = ApiKeyStore::open(&config.persistence.snapshot_dir)?;
        let (metadata, secret) = store.create(tenant, project, scopes, None)?;
        println!("API key created (save the secret now; it is shown only once):");
        println!("  key id:  {}", metadata.id);
        println!("  tenant:  {}", metadata.tenant_id);
        println!("  project: {}", metadata.project_id);
        println!("  secret:  {}", secret);
        return Ok(());
    }

    println!(
        "config: file={}, {} overrides from env, {} from CLI",
        resolved
            .file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<none>".to_string()),
        resolved.env_overrides,
        resolved.cli_overrides
    );

    let socket_addr = config.listen_addr;
    let auth_required = config.security.password.is_some();
    let tls_enabled = config.security.tls_enabled;

    if socket_addr.ip().is_unspecified() && !auth_required {
        let warning = "WARNING: binding to a wildcard address with NO authentication — \
                       the data store is exposed to the network. Set a password \
                       (--password / CALOD_SECURITY__PASSWORD / [security] password) \
                       or bind to 127.0.0.1.";
        eprintln!("{}", warning);
        tracing::warn!("{}", warning);
    }

    // Fail-closed: TLS enabled but misconfigured aborts startup here, before
    // any socket is bound — never a silent plaintext fallback.
    let tls_acceptor = build_tls_acceptor(&config.security)?;

    // Optional plaintext side-channel for local tooling: when TLS is enabled
    // and `tls_plaintext_fallback` is set, also serve plaintext on
    // `listen_addr` port + 1. Default is TLS-only.
    let plaintext_fallback_addr = if tls_enabled && config.security.tls_plaintext_fallback {
        let port = socket_addr.port().checked_add(1).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "tls_plaintext_fallback requires a free port at listen_addr port + 1",
            )
        })?;
        let addr = SocketAddr::new(socket_addr.ip(), port);
        let warning = format!(
            "WARNING: TLS plaintext fallback is ENABLED — unencrypted traffic is \
             accepted on {}. Intended for local tooling only.",
            addr
        );
        eprintln!("{}", warning);
        tracing::warn!("{}", warning);
        Some(addr)
    } else {
        None
    };

    let listener = bind_listener(socket_addr, &config)?;
    let plaintext_listener = match plaintext_fallback_addr {
        Some(addr) => Some(bind_listener(addr, &config)?),
        None => None,
    };

    let host = socket_addr.ip().to_string();
    let port = socket_addr.port();
    print_banner(&host, port, auth_required, tls_enabled);

    // Builds the store with fail-closed snapshot/AOF recovery; aborts startup
    // on recovery failure unless `allow_data_loss` is set.
    let pool = create_pool_from_config(config.clone()).await?;

    tracing::info!(
        "Server started on {} (TLS {})",
        socket_addr,
        if tls_acceptor.is_some() {
            "enabled"
        } else {
            "disabled"
        }
    );

    let tcp_keepalive_secs = config.advanced.tcp_keepalive_secs;

    // Global connection tracking
    let active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>> = Arc::new(
        DashMap::with_capacity_and_hasher(1024, ahash::RandomState::new()),
    );

    // Create channel for shutdown signal
    let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);

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

    let mut server_tasks = JoinSet::new();

    // REST action-ledger API (P2): same TLS posture as the RESP listener and
    // the store's fail-closed recovery policy (`allow_data_loss` quarantines
    // a corrupt ledger directory and starts empty, like the store's WAL).
    if config.api.enabled {
        let ledger_dir = config.api.ledger_data_dir(&config.persistence);
        let durability = config.api.ledger_durability(&config.persistence);
        let retention = config
            .api
            .retention_max_age_days
            .map(|days| calod_api::RetentionPolicy {
                max_age_ms: (days * 86_400_000) as i64,
            });
        let ledger = calod_api::open_ledger_with_retention(
            &ledger_dir,
            durability,
            config.allow_data_loss,
            retention,
        )
        .await?;
        if config.api.retention_apply_on_startup {
            match ledger.apply_configured_retention().await {
                Ok(report) => tracing::info!(
                    "retention on startup: {} runs, {} effects, {} events pruned (cutoff {})",
                    report.runs_removed,
                    report.effects_removed,
                    report.events_removed,
                    report.cutoff_ms
                ),
                Err(err) => tracing::error!("retention on startup failed: {}", err),
            }
        }
        let export_key = calod_api::load_or_create_export_key(&ledger_dir)
            .map_err(|e| format!("export signing key: {e}"))?;
        let api_state =
            calod_api::AppState::new(ledger.clone(), pool.store().api_key_store(), VERSION)
                .with_export_key(export_key);
        let api_listener = bind_listener(config.api.listen_addr, &config)?;
        let api_tls = tls_acceptor.clone();
        let mut api_shutdown = shutdown_tx.subscribe();
        server_tasks.spawn(async move {
            let serve = calod_api::serve(api_listener, api_tls, api_state, async move {
                let _ = api_shutdown.recv().await;
            });
            if let Err(err) = serve.await {
                tracing::error!("REST API server failed: {}", err);
            }
            // Drain and fsync the ledger WAL before the process exits.
            if let Err(err) = ledger.flush().await {
                tracing::error!("Ledger flush on shutdown failed: {}", err);
            }
        });
    }

    let plaintext_shutdown = shutdown_tx.subscribe();
    if let Some(plaintext_listener) = plaintext_listener {
        let addr = plaintext_fallback_addr.expect("listener implies address");
        let fallback_pool = pool.clone();
        let fallback_connections = active_connections.clone();
        server_tasks.spawn(async move {
            tracing::info!("Plaintext fallback listener started on {}", addr);
            accept_loop(
                plaintext_listener,
                None,
                fallback_pool,
                fallback_connections,
                tcp_keepalive_secs,
                plaintext_shutdown,
            )
            .await;
        });
    }

    let main_shutdown = shutdown_tx.subscribe();
    server_tasks.spawn(async move {
        accept_loop(
            listener,
            tls_acceptor,
            pool,
            active_connections,
            tcp_keepalive_secs,
            main_shutdown,
        )
        .await;
    });

    while let Some(join_result) = server_tasks.join_next().await {
        if let Err(e) = join_result {
            tracing::error!("Server task join error: {}", e);
        }
    }

    tracing::info!("Server shutdown complete");
    Ok(())
}

fn bind_listener(
    socket_addr: SocketAddr,
    config: &calod::store::config::CalodConfig,
) -> std::io::Result<TcpListener> {
    let socket = if socket_addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };

    socket.set_reuseaddr(config.advanced.reuse_address)?;
    socket.bind(socket_addr)?;
    socket.listen(config.advanced.tcp_backlog)
}

/// Serve one accepted client stream (plaintext or already-negotiated TLS)
/// through the shared command path.
async fn handle_client<S>(
    stream: S,
    addr: SocketAddr,
    pool: Pool,
    active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let client_id = Uuid::new_v4();

    if let Ok(conn) = pool.get().await {
        let connection: Connection = {
            let mut pooled_connection = conn.lock().await;
            pooled_connection.reset_for_client(client_id, addr);
            pooled_connection.clone()
        };
        let conn_arc = Arc::new(Mutex::new(connection.clone()));

        // Register the connection
        active_connections.insert(client_id, Arc::downgrade(&conn_arc));
        pool.register_connection(conn_arc.clone());

        // Process the connection without holding the active-connection mutex.
        connection
            .process(
                stream,
                active_connections.clone(),
                conn_arc.clone(),
                Some(&pool.connection_semaphore()),
            )
            .await;

        // Clean up the connection
        active_connections.remove(&client_id);
        pool.unregister_connection(client_id);
        tracing::debug!("Connection cleaned up for {}", addr);
    } else {
        tracing::error!("Failed to get connection from pool for {}", addr);
    }
}

/// Accept loop for one listener. When `tls_acceptor` is set, every accepted
/// TCP connection completes a TLS handshake before reaching the command path;
/// a failed handshake is logged and dropped, never served as plaintext.
async fn accept_loop(
    listener: TcpListener,
    tls_acceptor: Option<TlsAcceptor>,
    pool: Pool,
    active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
    tcp_keepalive_secs: u64,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) {
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
                        ka = ka.with_time(Duration::from_secs(tcp_keepalive_secs));
                        ka = ka.with_interval(Duration::from_secs(tcp_keepalive_secs));
                        if let Err(e) = socket_ref.set_tcp_keepalive(&ka) {
                            tracing::warn!("Failed to set TCP keepalive: {}", e);
                        }

                        if let Err(e) = stream.set_nodelay(true) {
                            tracing::warn!("Failed to set TCP_NODELAY: {}", e);
                        }

                        let tls_acceptor = tls_acceptor.clone();
                        let pool_clone = pool.clone();
                        let active_connections_clone = active_connections.clone();

                        handles.spawn(async move {
                            match tls_acceptor {
                                Some(acceptor) => match acceptor.accept(stream).await {
                                    Ok(tls_stream) => {
                                        handle_client(tls_stream, addr, pool_clone, active_connections_clone).await;
                                    }
                                    Err(e) => {
                                        tracing::warn!("TLS handshake with {} failed: {}", addr, e);
                                    }
                                },
                                None => {
                                    handle_client(stream, addr, pool_clone, active_connections_clone).await;
                                }
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
}
