
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use ahash::RandomState;
use bytes::{Buf, BytesMut};
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex, Semaphore};
use uuid::Uuid;

use crate::parser::parser::RESPParser;
use crate::parser::errors::RespError;
use crate::parser::value::Value;

use super::auth::AuthManager;
use super::command::Command;
use super::metrics::{ConnectionMetrics, MetricsAggregator};
use super::served_store::ServedCalodStore;
use tracing::{debug, error, info, instrument, warn};

type ClientId = Uuid;


#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionState {
    New,
    Authenticated,
    Closing,
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
            read_timeout_ms: 30_000,   // 30 second read timeout
            write_timeout_ms: 10_000,  // 10 second write timeout
            max_request_size: 512 * 1024 * 1024,  // 512MB max request size (similar to Redis defaults)
            initial_buffer_size: 16384, // 16KB initial buffer
            tls_enabled: false,       // TLS disabled by default
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
    state: ConnectionState,
    created_at: Instant,
    last_activity: Arc<Mutex<Instant>>,
    metrics: Arc<ConnectionMetrics>,
    config: ConnectionConfig,
    auth_manager: Arc<AuthManager>
}

impl Connection {

    pub fn new(
        store: Arc<ServedCalodStore>,
        config: ConnectionConfig,
        metrics_aggregator: Arc<MetricsAggregator>,
        auth_manager: Arc<AuthManager>,
    ) -> Self {
        let metrics = metrics_aggregator.new_connection_metrics();

        Self {
            store,
            buffer: BytesMut::with_capacity(config.initial_buffer_size),
            client_id: Uuid::new_v4(),
            connection_name: Arc::new(Mutex::new(None)),
            peer_addr: None,
            state: ConnectionState::New,
            created_at: Instant::now(),
            last_activity: Arc::new(Mutex::new(Instant::now())),
            metrics,
            config,
            auth_manager
        }
    }

    #[inline]
    pub fn get_peer_address(&self) -> Result<SocketAddr, Error> {
       self.peer_addr.ok_or_else(|| Error::new(ErrorKind::AddrNotAvailable, "Peer address not available."))
    }

    #[inline]
    pub fn set_peer_address(&mut self, addr: SocketAddr) {
        self.peer_addr = Some(addr);
    }

    #[inline]
    pub fn set_client_id(&mut self, client_id: ClientId) {
        self.client_id = client_id;
    }

    #[inline]
    pub fn get_client_id(&self) -> ClientId {
        self.client_id
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

    #[instrument(level = "debug", skip(self, stream, active_connections), fields(client_addr = %stream.peer_addr().unwrap(), client_id = %self.client_id))]
    pub async fn process(
        &mut self,
        stream: TcpStream,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
        connection_limit: Option<&Semaphore>
    ) {
        self.metrics.connection_established();
        
        if let Ok(addr) = stream.peer_addr() {
            self.peer_addr = Some(addr);
            info!("Processing connection from {} with client ID {}", addr, self.client_id);
        }
        
        // create a shared reference to the current connection
        let current_connection_arc_clone = Arc::new(Mutex::new(self.clone()));
        // low latency flag
        if let Err(e) = stream.set_nodelay(true) {
            warn!("Failed to set TCP_NODELAY: {}", e);
        }

        let (reader, writer) = stream.into_split();

        // 32 kB write buffer
        let writer = BufWriter::with_capacity(32768, writer);
        // buffer up to 1000 commands
        let (cmd_tx, cmd_rx) = mpsc::channel(1000);

        let read_task = tokio::spawn(self.clone().reader_task(
            reader,
            cmd_tx,
            current_connection_arc_clone.clone()
        ));

        let write_task = tokio::spawn(self.clone().writer_task(
            writer,
            cmd_rx,
            active_connections.clone(),
            current_connection_arc_clone
        ));

        tokio::select! {
            _ = read_task => {
                debug!("Read task completed first");
            }
            _ = write_task => {
                debug!("Write task completed first");
            }
        }

        self.metrics.connection_closed();

        if let Some(semaphore) = connection_limit {
            semaphore.add_permits(1);
        }

        info!("Connection with client ID {} closed.", self.client_id);
    }


    /// Reader task continuously reads data and parses commands
    async fn reader_task<R: AsyncReadExt + Unpin>(
        mut self,
        mut reader: R,
        cmd_tx: mpsc::Sender<(Command, usize, Instant)>,
        connection_arc: Arc<Mutex<Connection>>
    ) -> Result<(), Error> {

        let _read_timeout = Duration::from_millis(self.config.read_timeout_ms);
        let max_size = self.config.max_request_size;

        // main read loop
        loop {
            // make sure we have capacity in the buffer
            if self.buffer.len() >= max_size {
                error!("Request exceeds maximum allowed size of {} bytes", max_size);
                return Err(Error::new(ErrorKind::InvalidData, "Maximum request size exceeded"));
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
                },
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

                        if self.state == ConnectionState::New && !self.is_auth_command(&cmd) && self.auth_manager.auth_required() {
                            let err_cmd = Command::Unknown("NOAUTH Authentication required.".to_string());
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
                    },
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
        matches!(cmd, Command::Auth {..})
    }


    /// Writer task processed comands and sends responses
    async fn writer_task<W: AsyncWriteExt + Unpin>(
        self,
        mut writer: W,
        mut cmd_rx: mpsc::Receiver<(Command, usize, Instant)>,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
        connection_arc: Arc<Mutex<Connection>>
    ) -> Result<(), Error> {
        while let Some((command, _, received_at)) = cmd_rx.recv().await {
            // execute the command
            let response = self.store.execute(command, active_connections.clone(), connection_arc.clone()).await;

            // record command exec time
            let cmd_duration = received_at.elapsed();
            self.metrics.command_executed(cmd_duration);

            // write response to buffer
            match writer.write_all(&response).await {
                Ok(_) => {
                    self.metrics.bytes_sent(response.len() as u64);
                },
                Err(e) => {
                    error!("Write error: {}", e);
                    return Err(e);
                }
            }
        }

        // flush the buffer
        writer.flush().await?;
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
                    _ => std::slice::from_ref(&resp)
                };
                
                match Command::try_from(command_values) {
                    Ok(cmd) => Some((cmd, consumed)),
                    Err(_) => {
                        error!("Failed to convert RESP value to Command: {:?}", resp);
                        None
                    },
                }
            },
            Err(RespError::Incomplete) => None,
            Err(e) => {
                error!("Parsing error: {:?}", e);
                None
            },
        }
    }
}