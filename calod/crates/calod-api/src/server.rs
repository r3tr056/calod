//! Server startup: ledger opening with the store's fail-closed policy, and
//! the axum accept loop (plaintext or TLS — never a silent downgrade).

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};

use calod_core::store::config::FsyncStrategy;
use calod_ledger::{Ledger, LedgerConfig, LedgerError, RetentionPolicy, SystemClock};
use calod_storage::wal::WalConfig;

use crate::routes::{router, AppState};

/// Open (or create) the ledger under `data_dir`, honoring the store's
/// fail-closed recovery policy: a corrupt WAL or snapshot refuses startup
/// unless `allow_data_loss` is set, in which case the whole ledger directory
/// is quarantined (renamed, never deleted) and a fresh, empty ledger opens.
pub async fn open_ledger(
    data_dir: &Path,
    durability: FsyncStrategy,
    allow_data_loss: bool,
) -> Result<Arc<Ledger>, LedgerError> {
    open_ledger_with_retention(data_dir, durability, allow_data_loss, None).await
}

/// [`open_ledger`] with a retention policy (Phase 1).
pub async fn open_ledger_with_retention(
    data_dir: &Path,
    durability: FsyncStrategy,
    allow_data_loss: bool,
    retention: Option<RetentionPolicy>,
) -> Result<Arc<Ledger>, LedgerError> {
    match try_open(data_dir, durability.clone(), retention).await {
        Ok(ledger) => Ok(ledger),
        Err(err) => {
            if allow_data_loss {
                error!(
                    "Ledger recovery failed and `allow_data_loss` is set — quarantining {} and \
                     starting with an EMPTY ledger. Error was: {}",
                    data_dir.display(),
                    err
                );
                quarantine_dir(data_dir)?;
                try_open(data_dir, durability, retention).await
            } else {
                error!(
                    "Ledger recovery failed, refusing to start (set `allow_data_loss` / \
                     --allow-data-loss to quarantine it and start empty instead): {}",
                    err
                );
                Err(err)
            }
        }
    }
}

async fn try_open(
    data_dir: &Path,
    durability: FsyncStrategy,
    retention: Option<RetentionPolicy>,
) -> Result<Arc<Ledger>, LedgerError> {
    let wal = WalConfig::new(data_dir.join("wal")).with_fsync_strategy(durability);
    let config = LedgerConfig::new(data_dir)
        .with_wal_config(wal)
        .with_retention(retention);
    Ledger::open(config, Arc::new(SystemClock))
        .await
        .map(Arc::new)
}

/// Rename `dir` to `<dir>.corrupt-<unix-time>` (never delete), mirroring
/// [`calod_core::store::api_keys::ApiKeyStore::quarantine`].
fn quarantine_dir(dir: &Path) -> Result<(), LedgerError> {
    if !dir.exists() {
        return Ok(());
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let target: PathBuf = dir.with_file_name(format!(
        "{}.corrupt-{}",
        dir.file_name().and_then(|n| n.to_str()).unwrap_or("ledger"),
        secs
    ));
    std::fs::rename(dir, &target).map_err(|err| {
        LedgerError::Persistence(calod_core::store::error::PersistenceError::Io(err))
    })?;
    warn!(
        "Ledger quarantine: moved unrecoverable {} to {}",
        dir.display(),
        target.display()
    );
    Ok(())
}

/// Serve the REST API on `listener` until `shutdown` resolves, then return so
/// the caller can flush/close the ledger.
///
/// When `tls` is set every connection completes a TLS handshake before HTTP —
/// the API is TLS-only whenever the server has TLS enabled, exactly like the
/// RESP listener. A failed handshake is logged and dropped, never served as
/// plaintext.
pub async fn serve(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let app = router(state);
    match (tls, listener.local_addr()) {
        (Some(acceptor), Ok(addr)) => {
            info!("REST API (TLS) listening on {addr}");
            axum::serve(TlsListener::new(listener, acceptor), app)
                .with_graceful_shutdown(shutdown)
                .await
        }
        (Some(acceptor), Err(_)) => {
            axum::serve(TlsListener::new(listener, acceptor), app)
                .with_graceful_shutdown(shutdown)
                .await
        }
        (None, Ok(addr)) => {
            info!("REST API listening on {addr}");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
        }
        (None, Err(_)) => {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
        }
    }
}

/// axum [`axum::serve::Listener`] that TLS-handshakes each accepted TCP
/// connection before handing the stream to hyper. Handshake failures are
/// logged and the connection dropped; the accept loop keeps going.
struct TlsListener {
    inner: TcpListener,
    acceptor: TlsAcceptor,
}

impl TlsListener {
    fn new(inner: TcpListener, acceptor: TlsAcceptor) -> Self {
        Self { inner, acceptor }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, addr)) => match self.acceptor.accept(stream).await {
                    Ok(tls_stream) => return (tls_stream, addr),
                    Err(err) => {
                        warn!("REST API TLS handshake with {addr} failed: {err}");
                    }
                },
                Err(err) => {
                    error!("REST API accept error: {err}");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}
