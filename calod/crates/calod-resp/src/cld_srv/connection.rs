use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use ahash::RandomState;
use bytes::{Buf, BytesMut};
use dashmap::DashMap;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, Mutex, Semaphore};
use uuid::Uuid;

use crate::parser::errors::RespError;
use crate::parser::parser::RESPParser;
use crate::parser::value::Value;

use super::auth::AuthManager;
use super::command::Command;
use super::metrics::{ConnectionMetrics, MetricsAggregator};
use super::protocol::{ConnCtx, ProtocolVersion};
use super::served_store::ServedCalodStore;
use tracing::{debug, error, instrument};

type ClientId = Uuid;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionState {
    New,
    Authenticated,
    Closing,
}

/// Per-connection transaction state for MULTI/EXEC/WATCH.
///
/// `watched` holds `(key, version_at_watch)` recorded by WATCH (versions come
/// from the store's `WatchRegistry`). `queued` is `Some` once MULTI begins and
/// accumulates commands until EXEC/DISCARD. `dirty` is set if a queued command
/// failed to parse, forcing EXEC to abort with EXECABORT.
#[derive(Default)]
pub struct TxnState {
    pub watched: Vec<(String, u64)>,
    pub queued: Option<Vec<Command>>,
    pub dirty: bool,
}

#[derive(Clone)]
pub struct ConnectionConfig {
    pub read_timeout_ms: u64,
    pub write_timeout_ms: u64,
    pub max_request_size: usize,
    pub initial_buffer_size: usize,
    pub tls_enabled: bool,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            read_timeout_ms: 30_000,             // 30 second read timeout
            write_timeout_ms: 10_000,            // 10 second write timeout
            max_request_size: 512 * 1024 * 1024, // 512MB max request size (similar to Redis defaults)
            initial_buffer_size: 16384,          // 16KB initial buffer
            tls_enabled: false,                  // TLS disabled by default
        }
    }
}

#[derive(Clone)]
pub struct Connection {
    store: Arc<ServedCalodStore>,
    buffer: BytesMut,
    client_id: ClientId,
    connection_name: Arc<Mutex<Option<String>>>,
    peer_addr: Option<std::net::SocketAddr>,
    state: Arc<Mutex<ConnectionState>>,
    created_at: Instant,
    last_activity: Arc<Mutex<Instant>>,
    metrics: Arc<ConnectionMetrics>,
    config: ConnectionConfig,
    auth_manager: Arc<AuthManager>,
    /// Negotiated RESP protocol version (2 or 3), shared across this
    /// connection's reader/writer clones so a `HELLO 3` on one is visible to the
    /// other without locking. Read on the hot path via [`Connection::ctx`].
    proto: Arc<AtomicU8>,
    /// Currently `SELECT`ed database index (0-based), shared across clones so a
    /// `SELECT` is visible to subsequent commands. Read lock-free in
    /// [`Connection::ctx`].
    db_index: Arc<AtomicU8>,
    /// Out-of-band server→client push channel for pub/sub messages and keyspace
    /// notifications. The sender is cloned into the `PubSubRegistry` on
    /// SUBSCRIBE; the receiver is taken once by the writer task. Shared across
    /// clones so a clone reached via `connection_arc` can hand out the sender.
    push_tx: mpsc::Sender<Vec<u8>>,
    push_rx: Arc<Mutex<Option<mpsc::Receiver<Vec<u8>>>>>,
    /// Count of active subscriptions (channels + patterns). Read lock-free via
    /// the shared atomic to gate RESP2 subscriber mode in [`Connection::ctx`].
    sub_total: Arc<AtomicUsize>,
    /// MULTI/EXEC/WATCH state, shared across this connection's clones so that
    /// `execute` (reached via `connection_arc`) and teardown see the same state.
    txn: Arc<Mutex<TxnState>>,
}

impl Connection {
    pub fn new(
        store: Arc<ServedCalodStore>,
        config: ConnectionConfig,
        metrics_aggregator: Arc<MetricsAggregator>,
        auth_manager: Arc<AuthManager>,
    ) -> Self {
        let metrics = metrics_aggregator.new_connection_metrics();
        let (push_tx, push_rx) = mpsc::channel(1024);

        Self {
            store,
            buffer: BytesMut::with_capacity(config.initial_buffer_size),
            client_id: Uuid::new_v4(),
            connection_name: Arc::new(Mutex::new(None)),
            peer_addr: None,
            state: Arc::new(Mutex::new(ConnectionState::New)),
            created_at: Instant::now(),
            last_activity: Arc::new(Mutex::new(Instant::now())),
            metrics,
            config,
            auth_manager,
            proto: Arc::new(AtomicU8::new(ProtocolVersion::Resp2.as_u8())),
            db_index: Arc::new(AtomicU8::new(0)),
            push_tx,
            push_rx: Arc::new(Mutex::new(Some(push_rx))),
            sub_total: Arc::new(AtomicUsize::new(0)),
            txn: Arc::new(Mutex::new(TxnState::default())),
        }
    }

    #[inline]
    pub fn get_peer_address(&self) -> Result<SocketAddr, Error> {
        self.peer_addr
            .ok_or_else(|| Error::new(ErrorKind::AddrNotAvailable, "Peer address not available."))
    }

    #[inline]
    pub fn set_peer_address(&mut self, addr: SocketAddr) {
        self.peer_addr = Some(addr);
    }

    #[inline]
    pub fn set_client_id(&mut self, client_id: ClientId) {
        self.client_id = client_id;
    }

    pub fn reset_for_client(&mut self, client_id: ClientId, addr: SocketAddr) {
        self.buffer.clear();
        self.client_id = client_id;
        self.peer_addr = Some(addr);
        self.connection_name = Arc::new(Mutex::new(None));
        self.state = Arc::new(Mutex::new(ConnectionState::New));
        self.created_at = Instant::now();
        self.last_activity = Arc::new(Mutex::new(Instant::now()));
        // Fresh per-client protocol atomic at RESP2; pooled connections are
        // reused, so this must not inherit the previous client's negotiation.
        self.proto = Arc::new(AtomicU8::new(ProtocolVersion::Resp2.as_u8()));
        // Reset to database 0; pooled connections must not inherit a SELECT.
        self.db_index = Arc::new(AtomicU8::new(0));
        // Fresh push channel and subscription counter per client.
        let (push_tx, push_rx) = mpsc::channel(1024);
        self.push_tx = push_tx;
        self.push_rx = Arc::new(Mutex::new(Some(push_rx)));
        self.sub_total = Arc::new(AtomicUsize::new(0));
        self.txn = Arc::new(Mutex::new(TxnState::default()));
    }

    /// A handle to this connection's transaction state (shared across clones).
    #[inline]
    pub fn txn_handle(&self) -> Arc<Mutex<TxnState>> {
        self.txn.clone()
    }

    /// A clone of this connection's push-message sender, for the
    /// [`PubSubRegistry`] to deliver out-of-band messages to the writer task.
    #[inline]
    pub fn push_sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.push_tx.clone()
    }

    /// Record the current number of active subscriptions (channels + patterns).
    #[inline]
    pub fn set_subscription_total(&self, n: usize) {
        self.sub_total.store(n, Ordering::Relaxed);
    }

    /// The current number of active subscriptions.
    #[inline]
    pub fn subscription_total(&self) -> usize {
        self.sub_total.load(Ordering::Relaxed)
    }

    /// The protocol version negotiated for this connection (RESP2 until a
    /// successful `HELLO 3`).
    #[inline]
    pub fn protocol(&self) -> ProtocolVersion {
        ProtocolVersion::load(&self.proto)
    }

    /// Set the negotiated protocol version. Visible to all clones of this
    /// connection because the backing atomic is shared via `Arc`.
    #[inline]
    pub fn set_protocol(&self, proto: ProtocolVersion) {
        proto.store(&self.proto);
    }

    /// The currently `SELECT`ed database index for this connection.
    #[inline]
    pub fn db_index(&self) -> u8 {
        self.db_index.load(Ordering::Relaxed)
    }

    /// Set the `SELECT`ed database index. Shared across clones via the atomic.
    #[inline]
    pub fn set_db_index(&self, db: u8) {
        self.db_index.store(db, Ordering::Relaxed);
    }

    /// A cheap `Copy` snapshot of the per-connection context for reply encoding
    /// and subscriber-mode gating. All reads are lock-free atomic loads.
    #[inline]
    pub fn ctx(&self) -> ConnCtx {
        ConnCtx {
            proto: self.protocol(),
            subscribed: self.subscription_total() > 0,
            db: self.db_index(),
        }
    }

    #[inline]
    pub fn get_client_id(&self) -> ClientId {
        self.client_id
    }

    /// A non-negative `u64` connection id for `CLIENT ID`, derived from the
    /// client UUID (Redis client ids are positive integers).
    #[inline]
    pub fn numeric_id(&self) -> u64 {
        self.client_id.as_u64_pair().0 & 0x7fff_ffff_ffff_ffff
    }

    #[inline]
    pub async fn get_client_name(&self) -> Option<String> {
        let lock = self.connection_name.lock().await;
        lock.clone()
    }

    #[inline]
    pub async fn set_client_name(&self, name: String) {
        let mut lock = self.connection_name.lock().await;
        *lock = Some(name);
    }

    #[inline]
    pub fn get_uptime(&self) -> Duration {
        self.created_at.elapsed()
    }

    #[inline]
    pub async fn get_idle_time(&self) -> Duration {
        let last = self.last_activity.lock().await;
        last.elapsed()
    }

    #[inline]
    pub async fn update_activity(&self) {
        let mut last = self.last_activity.lock().await;
        *last = Instant::now();
    }

    #[inline]
    pub async fn mark_closing(&self) {
        let mut state = self.state.lock().await;
        *state = ConnectionState::Closing;
    }

    #[inline]
    async fn current_state(&self) -> ConnectionState {
        *self.state.lock().await
    }

    #[inline]
    async fn set_authenticated(&self) {
        let mut state = self.state.lock().await;
        *state = ConnectionState::Authenticated;
    }

    /// Serve one client stream. Generic over any async byte stream so the same
    /// command path handles both plaintext `TcpStream`s and rustls TLS
    /// streams; TCP-specific setup (keepalive, TCP_NODELAY) and peer-address
    /// bookkeeping are the caller's responsibility.
    #[instrument(level = "debug", skip(self, stream, active_connections, connection_arc), fields(client_id = %self.client_id))]
    pub async fn process<S>(
        self,
        stream: S,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
        connection_arc: Arc<Mutex<Connection>>,
        connection_limit: Option<&Semaphore>,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        self.metrics.connection_established();

        if let Some(addr) = self.peer_addr {
            debug!(
                "Processing connection from {} with client ID {}",
                addr, self.client_id
            );
        }

        let (reader, writer) = tokio::io::split(stream);

        // 32 kB write buffer
        let writer = BufWriter::with_capacity(32768, writer);
        // buffer up to 1000 commands
        let (cmd_tx, cmd_rx) = mpsc::channel(1000);
        // Take the out-of-band push receiver for the writer task (once).
        let push_rx = self
            .push_rx
            .lock()
            .await
            .take()
            .expect("push receiver taken exactly once per connection");

        let read_task = tokio::spawn(self.clone().reader_task(
            reader,
            cmd_tx,
            connection_arc.clone(),
        ));

        let write_task = tokio::spawn(self.clone().writer_task(
            writer,
            cmd_rx,
            push_rx,
            active_connections.clone(),
            connection_arc,
        ));

        tokio::select! {
            _ = read_task => {
                debug!("Read task completed first");
            }
            _ = write_task => {
                debug!("Write task completed first");
            }
        }

        self.mark_closing().await;

        // Drop any pub/sub subscriptions so the registry doesn't retain a dead
        // sender keyed by this client id.
        self.store.pubsub_cleanup(self.client_id);

        // Release any WATCH versions held by this connection.
        {
            let txn = self.txn.lock().await;
            for (key, _) in &txn.watched {
                self.store.unwatch_key(key);
            }
        }

        self.metrics.connection_closed();

        let _ = connection_limit;

        debug!("Connection with client ID {} closed.", self.client_id);
    }

    /// Reader task continuously reads data and parses commands
    async fn reader_task<R: AsyncReadExt + Unpin>(
        mut self,
        mut reader: R,
        cmd_tx: mpsc::Sender<(Command, usize, Instant)>,
        connection_arc: Arc<Mutex<Connection>>,
    ) -> Result<(), Error> {
        let _read_timeout = Duration::from_millis(self.config.read_timeout_ms);
        let max_size = self.config.max_request_size;

        // main read loop
        loop {
            if self.current_state().await == ConnectionState::Closing {
                return Ok(());
            }

            // make sure we have capacity in the buffer
            if self.buffer.len() >= max_size {
                error!("Request exceeds maximum allowed size of {} bytes", max_size);
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "Maximum request size exceeded",
                ));
            }

            // Reserve space for incoming data - use larger chunks for better throughput
            let reserve_size = if self.buffer.capacity() - self.buffer.len() < 4096 {
                // Double the buffer when we're running out of space
                self.buffer.capacity()
            } else {
                // Otherwise use a fixed chunk size
                4096
            };

            self.buffer.reserve(reserve_size);

            let bytes_read = match reader.read_buf(&mut self.buffer).await {
                Ok(0) => {
                    debug!("Client disconnected - read returned 0 bytes");
                    return Ok(());
                }
                Ok(n) => n,
                Err(e) => {
                    error!("Read error: {}", e);
                    return Err(e);
                }
            };

            debug!("Read {} bytes", bytes_read);
            self.metrics.bytes_received(bytes_read as u64);

            let mut position = 0;
            while position < self.buffer.len() {
                match self.parse_command_at_position(position) {
                    Some((cmd, consumed)) => {
                        position += consumed;

                        self.metrics.command_received();

                        if let Command::Auth { password } = &cmd {
                            // Redis: AUTH with no server password configured is an error
                            // rather than a silent success.
                            if !self.auth_manager.auth_required() {
                                let err_cmd = Command::Unknown(
                                    "ERR Client sent AUTH, but no password is set. Did you mean AUTH <username> <password>?"
                                        .to_string(),
                                );
                                if cmd_tx.send((err_cmd, 0, Instant::now())).await.is_err() {
                                    return Ok(());
                                }
                                continue;
                            }
                            if !self.auth_manager.authenticate(password) {
                                let err_cmd = Command::Unknown(
                                    "WRONGPASS invalid username-password pair".to_string(),
                                );
                                if cmd_tx.send((err_cmd, 0, Instant::now())).await.is_err() {
                                    return Ok(());
                                }
                                continue;
                            }
                            self.set_authenticated().await;
                        }

                        // HELLO with inline AUTH authenticates exactly like AUTH;
                        // HELLO without AUTH on an auth-required, not-yet-
                        // authenticated connection is rejected with NOAUTH (the
                        // protocol switch only happens once authorized).
                        if let Command::Hello { auth, .. } = &cmd {
                            if let Some((_user, password)) = auth {
                                if !self.auth_manager.auth_required() {
                                    let err_cmd = Command::Unknown(
                                        "ERR Client sent AUTH, but no password is set. Did you mean AUTH <username> <password>?"
                                            .to_string(),
                                    );
                                    if cmd_tx.send((err_cmd, 0, Instant::now())).await.is_err() {
                                        return Ok(());
                                    }
                                    continue;
                                }
                                if !self.auth_manager.authenticate(password) {
                                    let err_cmd = Command::Unknown(
                                        "WRONGPASS invalid username-password pair".to_string(),
                                    );
                                    if cmd_tx.send((err_cmd, 0, Instant::now())).await.is_err() {
                                        return Ok(());
                                    }
                                    continue;
                                }
                                self.set_authenticated().await;
                            } else if self.current_state().await == ConnectionState::New
                                && self.auth_manager.auth_required()
                            {
                                let err_cmd = Command::Unknown(
                                    "NOAUTH HELLO must be called with the client already authenticated, otherwise the HELLO <proto> AUTH <user> <pass> option can be used to authenticate the client and select the RESP protocol version at the same time"
                                        .to_string(),
                                );
                                if cmd_tx.send((err_cmd, 0, Instant::now())).await.is_err() {
                                    return Ok(());
                                }
                                continue;
                            }
                        }

                        if self.current_state().await == ConnectionState::New
                            && !self.is_auth_command(&cmd)
                            && self.auth_manager.auth_required()
                        {
                            let err_cmd =
                                Command::Unknown("NOAUTH Authentication required.".to_string());
                            if cmd_tx.send((err_cmd, 0, Instant::now())).await.is_err() {
                                return Ok(()); // Channel closed, reader should exit
                            }
                            continue;
                        }

                        // update last activity time
                        let now = Instant::now();
                        if now.duration_since(self.created_at) > Duration::from_secs(60) {
                            let conn = connection_arc.lock().await;
                            conn.update_activity().await;
                        }
                        // send command to writer task
                        if cmd_tx.send((cmd, consumed, Instant::now())).await.is_err() {
                            return Ok(());
                        }
                    }
                    None => {
                        // Incomplete command, wait for more data
                        break;
                    }
                }
            }

            // compact the buffer by removing the processed data
            if position > 0 {
                if position == self.buffer.len() {
                    // clear the full buffer
                    self.buffer.clear();
                } else {
                    // advance to the read position
                    self.buffer.advance(position);
                }
            }
        }
    }

    #[inline]
    fn is_auth_command(&self, cmd: &Command) -> bool {
        matches!(
            cmd,
            Command::Auth { .. } | Command::Ping {} | Command::Hello { .. } | Command::Quit {}
        )
    }

    /// Writer task: services command replies AND out-of-band server pushes
    /// (pub/sub messages, keyspace notifications) for one connection.
    ///
    /// A `biased` `select!` drains command replies first so that a command's
    /// reply is never interleaved by a push frame mid-batch (Redis clients
    /// expect reply-before-message ordering). Pushes are flushed immediately.
    async fn writer_task<W: AsyncWriteExt + Unpin>(
        self,
        mut writer: W,
        mut cmd_rx: mpsc::Receiver<(Command, usize, Instant)>,
        mut push_rx: mpsc::Receiver<Vec<u8>>,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
        connection_arc: Arc<Mutex<Connection>>,
    ) -> Result<(), Error> {
        const FLUSH_THRESHOLD_BYTES: usize = 64 * 1024;
        const MAX_BATCH_COMMANDS: usize = 64;
        let write_timeout = Duration::from_millis(self.config.write_timeout_ms);

        loop {
            tokio::select! {
                biased;
                maybe_cmd = cmd_rx.recv() => {
                    let Some((command, _, received_at)) = maybe_cmd else {
                        break;
                    };
                    if self.current_state().await == ConnectionState::Closing {
                        break;
                    }

                    let mut pending = Some((command, received_at));
                    let mut pending_bytes = 0usize;
                    let mut processed = 0usize;
                    let mut channel_closed = false;

                    loop {
                        let (command, received_at) = match pending.take() {
                            Some(item) => item,
                            None => match cmd_rx.try_recv() {
                                Ok((command, _, received_at)) => (command, received_at),
                                Err(mpsc::error::TryRecvError::Empty) => break,
                                Err(mpsc::error::TryRecvError::Disconnected) => {
                                    channel_closed = true;
                                    break;
                                }
                            },
                        };

                        // Snapshot the protocol context per command so a HELLO
                        // appearing mid-pipeline switches subsequent replies.
                        let ctx = self.ctx();
                        let response = self
                            .store
                            .execute(
                                command,
                                active_connections.clone(),
                                connection_arc.clone(),
                                ctx,
                            )
                            .await;

                        let cmd_duration = received_at.elapsed();
                        self.metrics.command_executed(cmd_duration);

                        match tokio::time::timeout(write_timeout, writer.write_all(&response)).await {
                            Ok(Ok(())) => {
                                self.metrics.bytes_sent(response.len() as u64);
                                pending_bytes += response.len();
                                processed += 1;
                            }
                            Ok(Err(e)) => {
                                error!("Write error: {}", e);
                                return Err(e);
                            }
                            Err(_) => {
                                let e = Error::new(ErrorKind::TimedOut, "Timed out writing response");
                                error!("{}", e);
                                return Err(e);
                            }
                        }

                        if pending_bytes >= FLUSH_THRESHOLD_BYTES || processed >= MAX_BATCH_COMMANDS {
                            break;
                        }
                    }

                    if pending_bytes > 0 {
                        match tokio::time::timeout(write_timeout, writer.flush()).await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                error!("Flush error: {}", e);
                                return Err(e);
                            }
                            Err(_) => {
                                let e = Error::new(
                                    ErrorKind::TimedOut,
                                    "Timed out flushing response buffer",
                                );
                                error!("{}", e);
                                return Err(e);
                            }
                        }
                    }

                    if channel_closed {
                        break;
                    }
                }
                maybe_push = push_rx.recv() => {
                    // The writer's own `self` holds a `push_tx` clone, so the
                    // channel never closes here; `None` is treated defensively.
                    let Some(bytes) = maybe_push else { continue; };
                    match tokio::time::timeout(write_timeout, writer.write_all(&bytes)).await {
                        Ok(Ok(())) => self.metrics.bytes_sent(bytes.len() as u64),
                        Ok(Err(e)) => {
                            error!("Push write error: {}", e);
                            return Err(e);
                        }
                        Err(_) => {
                            let e = Error::new(ErrorKind::TimedOut, "Timed out writing push message");
                            error!("{}", e);
                            return Err(e);
                        }
                    }
                    match tokio::time::timeout(write_timeout, writer.flush()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            error!("Push flush error: {}", e);
                            return Err(e);
                        }
                        Err(_) => {
                            return Err(Error::new(
                                ErrorKind::TimedOut,
                                "Timed out flushing push message",
                            ));
                        }
                    }
                }
            }
        }

        let _ = tokio::time::timeout(write_timeout, writer.flush()).await;
        Ok(())
    }

    /// Parse a command starting at a given position in the connection buffer
    #[inline]
    fn parse_command_at_position(&self, start_pos: usize) -> Option<(Command, usize)> {
        if start_pos >= self.buffer.len() {
            return None;
        }

        match RESPParser::parse_value(&self.buffer[start_pos..]) {
            Ok((resp, consumed)) => {
                // Extract the command
                let command_values = match resp {
                    Value::Array(ref array_values) => array_values.as_slice(),
                    _ => std::slice::from_ref(&resp),
                };

                match Command::try_from(command_values) {
                    Ok(cmd) => Some((cmd, consumed)),
                    Err(e) => {
                        debug!(
                            "Failed to convert RESP value to Command: {:?} ({})",
                            resp, e
                        );
                        Some((Command::Unknown(format!("ERR {}", e)), consumed))
                    }
                }
            }
            Err(RespError::Incomplete) => None,
            Err(e) => {
                error!("Parsing error: {:?}", e);
                None
            }
        }
    }
}
