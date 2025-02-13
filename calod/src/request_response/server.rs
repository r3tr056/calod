
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::sync::{Arc, Weak};

use bytes::{Buf, BytesMut};
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::parser::parser::{RESPParser, RespError, Value};

use super::command::Command;
use super::served_store::ServedCalodStore;
use tracing::{debug, error, info, instrument, trace};

type ClientId = Uuid;

#[derive(Clone)]
pub struct Connection {
    store: Arc<ServedCalodStore>,
    buffer: BytesMut,
    client_id: ClientId,
    connection_name: Arc<Mutex<Option<String>>>,
    peer_addr: Option<std::net::SocketAddr>,
}

impl Connection {
    const INITIAL_BUFFER_SIZE: usize = 4096;

    pub fn new(store: Arc<ServedCalodStore>) -> Self {
        Self {
            store,
            buffer: BytesMut::with_capacity(Self::INITIAL_BUFFER_SIZE),
            client_id: Uuid::nil(),
            connection_name: Arc::new(Mutex::new(None)),
            peer_addr: None
        }
    }

    pub fn get_peer_address(&self) -> Result<SocketAddr, Error> {
       self.peer_addr.ok_or_else(|| Error::new(ErrorKind::AddrNotAvailable, "Peer address not available."))
    }

    pub fn set_client_id(&mut self, client_id: ClientId) {
        self.client_id = client_id;
    }

    pub fn get_client_id(&self) -> ClientId {
        self.client_id
    }

    pub async fn get_client_name(&self) -> Option<String> {
        let lock = self.connection_name.lock().await;
        lock.clone()
    }

    pub async fn set_client_name(&self, name: String) {
        let mut lock = self.connection_name.lock().await;
        *lock = Some(name);
    }

    #[instrument(level = "debug", skip(self, stream, active_connections), fields(client_addr = %stream.peer_addr().unwrap(), client_id = %self.client_id))]
    pub async fn process(&mut self, mut stream: TcpStream, active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>>>) {
        let client_addr = match stream.peer_addr() {
            Ok(addr) => addr,
            Err(e) => {
                error!("Failed to get peer address: {}", e);
                return;
            }
        };
        
        info!("Processing connection from {} with client ID {}", client_addr, self.client_id);
        let current_connection_arc_clone = Arc::new(Mutex::new(self.clone()));

        loop {
            self.buffer.reserve(Self::INITIAL_BUFFER_SIZE);

            match stream.read_buf(&mut self.buffer).await {
                Ok(0) => {
                    debug!("Client {} disconnected gracefully.", client_addr);
                    break;
                },
                Ok(bytes_read) => {
                    debug!("Read {} bytes from client {}: {:?}", bytes_read, client_addr, String::from_utf8_lossy(&self.buffer));
                    trace!("Raw command received from client {}: {:?}", client_addr, String::from_utf8_lossy(&self.buffer));

                    while let Some((command, consumed)) = self.parse_command() {
                        debug!("Parsed command from client {}: {:?}", client_addr, command);
                        let response = self.store.execute(command, active_connections.clone(), current_connection_arc_clone.clone()).await;
                        debug!("Response for client {} is ready", client_addr);
                        if let Err(e) = stream.write_all(&response).await {
                            error!("Write error to client {}: {}", client_addr, e);
                            break;
                        }
                        debug!("Response sent to client {}", client_addr);
                        self.buffer.advance(consumed);
                        debug!("Buffer advanced by {}, remaining buffer size: {}", consumed, self.buffer.len());
                    }
                },
                Err(e) => {
                    error!("Read error from client {}: {}", client_addr, e);
                    break;
                }
            }
        }
        if let Err(e) = stream.shutdown().await {
            error!("Error shutting down connection from {}: {}", client_addr, e);
        }
        info!("Connection with {} and client ID {} closed.", client_addr, self.client_id);
    }

    fn parse_command(&self) -> Option<(Command, usize)> {
        let mut cursor = 0;
        let mut commands = Vec::new();

        while cursor < self.buffer.len() {
            match RESPParser::parse_value(&self.buffer[cursor..]) {
                Ok((resp, consumed)) => {
                    // check if the parsed values is an array, and extract its elements
                    let command_values = match resp {
                        Value::Array(ref array_values) => array_values.as_slice(),
                        _ => std::slice::from_ref(&resp)
                    };
                    debug!("Parsed RESP value: {:?}", resp);
                    match Command::try_from(command_values) {
                        Ok(cmd) => {
                            commands.push((cmd, consumed));
                            cursor += consumed;
                            debug!("Command parsed successfully. consumed: {}", consumed);
                        },
                        Err(_) => {
                            error!("Failed to convert RESP value to Command: {:?}", resp);
                            break;
                        },
                    }
                }
                Err(RespError::Incomplete) => {
                    debug!("Incomplete data, waiting for more input");
                    break;
                },
                Err(e) => {
                    error!("Parsing error: {:?}", e);
                    return None;
                },
            }
        }

        commands.into_iter().next()
    }
}