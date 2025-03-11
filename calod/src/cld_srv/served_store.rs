use std::{net::IpAddr, sync::{Arc, Weak}};

use ahash::{AHashMap, RandomState};
use bytes::{BufMut, BytesMut};
use dashmap::DashMap;
use tokio::sync::Mutex;
use tracing::{debug, error, info, instrument, trace, warn};
use uuid::Uuid;

use crate::store::{calod_data::DataType, config::CalodConfig, error::CacheError, sharded_store::ShardedStore};
use super::{command::{ClientSubCommand, Command}, connection::Connection};

type ClientId = Uuid;

const PONG_RESPONSE: &[u8] = b"+PONG\r\n";
const OK_RESPONSE: &[u8] = b"+OK\r\n";
const NIL_RESPONSE: &[u8] = b"$-1\r\n";

pub struct ServedCalodStore {
    store: Arc<ShardedStore>,
    command_response_cache: AHashMap<&'static str, Vec<u8>>,
}

fn parse_ip_port(ip_port_str: &str) -> Result<(IpAddr, u16), String> {
    let (ip_str, port_str) = ip_port_str.split_once(':').ok_or_else(|| "Invalid ip:port fromat".to_string())?;

    let ip_addr: IpAddr = ip_str.parse().map_err(|_| "Invalid IP address".to_string())?;
    let port: u16 = port_str.parse().map_err(|_| "Invalid port number".to_string())?;

    Ok((ip_addr, port))
}

impl ServedCalodStore {
    pub fn new() -> Self {
        let config = CalodConfig::default();
        let mut command_response_cache = AHashMap::new();

        command_response_cache.insert("COMMAND", Self::precompute_command_resp());
        command_response_cache.insert("COMMAND_LIST", Self::precompute_command_list_resp());
        
        ServedCalodStore {
            store: Arc::new(ShardedStore::new(config)),
            command_response_cache
        }
    }

    /// Pre-allocate buffer with reasonable initial capacity based on expected response size
    #[inline]
    fn allocate_buffer(expected_size: usize) -> BytesMut {
        BytesMut::with_capacity(expected_size.max(128))
    }

    /// Static responses to avoid allocations
    #[inline]
    fn static_response(&self, response: &'static [u8]) -> Vec<u8> {
        response.to_vec()
    }

    // fn wrap_simple_response(&self, status: &str) -> Vec<u8> {
    //     if status == "PONG" {
    //         return self.static_response(PONG_RESPONSE);
    //     } else if status == "OK" {
    //         return self.static_response(OK_RESPONSE);
    //     }

    //     let required_size = 1 + status.len() + 2;
    //     let mut buffer = BytesMut::with_capacity(required_size);
    //     buffer.put_u8(b'+');
    //     buffer.extend_from_slice(status.as_bytes());
    //     buffer.extend_from_slice(b"\r\n");
    //     buffer.freeze().to_vec()
    // }
    
    fn wrap_response(&self, data: Result<DataType, CacheError>) -> Vec<u8> {
        match data {
            Ok(data_type) => match data_type {
                DataType::String(s) => {
                    trace!("Wrapping String response: {}", s);
                    // Estimate exact buffer size needed to avoid reallocations
                    let size_str = s.len().to_string();
                    let buffer_size = 1 + size_str.len() + 2 + s.len() + 2; // '$' + len + '\r\n' + content + '\r\n'
                    let mut buffer = Self::allocate_buffer(buffer_size);
                    
                    buffer.put_u8(b'$');
                    buffer.extend_from_slice(size_str.as_bytes());
                    buffer.extend_from_slice(b"\r\n");
                    buffer.extend_from_slice(s.as_bytes());
                    buffer.extend_from_slice(b"\r\n");
                    
                    buffer.to_vec()
                },
                DataType::List(items) => {
                    trace!("Wrapping List response, {} items", items.len());
                    // Estimate buffer size based on item count and average item size
                    let avg_item_size = items.iter().map(|s| s.len()).sum::<usize>().checked_div(items.len().max(1)).unwrap_or(16);
                    let buffer_size = 1 + items.len().to_string().len() + 2 + (items.len() * (1 + 10 + 2 + avg_item_size + 2));
                    let mut buffer = Self::allocate_buffer(buffer_size);
                    
                    buffer.put_u8(b'*');
                    buffer.extend_from_slice(items.len().to_string().as_bytes());
                    buffer.extend_from_slice(b"\r\n");
                    
                    for item in items {
                        buffer.put_u8(b'$');
                        buffer.extend_from_slice(item.len().to_string().as_bytes());
                        buffer.extend_from_slice(b"\r\n");
                        buffer.extend_from_slice(item.as_bytes());
                        buffer.extend_from_slice(b"\r\n");
                    }
                    
                    buffer.to_vec()
                }
                DataType::Set(items) => {
                    trace!("Wrapping Set response, {} items", items.len());
                    // Similar estimation as lists
                    let avg_item_size = items.iter().map(|s| s.len()).sum::<usize>().checked_div(items.len().max(1)).unwrap_or(16);
                    let buffer_size = 1 + items.len().to_string().len() + 2 + (items.len() * (1 + 10 + 2 + avg_item_size + 2));
                    let mut buffer = Self::allocate_buffer(buffer_size);
                    
                    buffer.put_u8(b'*');
                    buffer.extend_from_slice(items.len().to_string().as_bytes());
                    buffer.extend_from_slice(b"\r\n");
                    
                    for item in items {
                        buffer.put_u8(b'$');
                        buffer.extend_from_slice(item.len().to_string().as_bytes());
                        buffer.extend_from_slice(b"\r\n");
                        buffer.extend_from_slice(item.as_bytes());
                        buffer.extend_from_slice(b"\r\n");
                    }
                    
                    buffer.to_vec()
                },
                DataType::Hash(map) => {
                    trace!("Wrapping Hash response, {} entries", map.len());
                    // Hash maps have 2× elements (key+value pairs)
                    let size = map.len() * 2;
                    let avg_entry_size = 16; // Estimate average key/value size
                    let buffer_size = 1 + size.to_string().len() + 2 + (size * (1 + 10 + 2 + avg_entry_size + 2));
                    let mut buffer = Self::allocate_buffer(buffer_size);
                    
                    buffer.put_u8(b'*');
                    buffer.extend_from_slice((map.len() * 2).to_string().as_bytes());
                    buffer.extend_from_slice(b"\r\n");
                    
                    for (k, v) in map {
                        // Write key
                        buffer.put_u8(b'$');
                        buffer.extend_from_slice(k.len().to_string().as_bytes());
                        buffer.extend_from_slice(b"\r\n");
                        buffer.extend_from_slice(k.as_bytes());
                        buffer.extend_from_slice(b"\r\n");
                        
                        // Write value
                        buffer.put_u8(b'$');
                        buffer.extend_from_slice(v.len().to_string().as_bytes());
                        buffer.extend_from_slice(b"\r\n");
                        buffer.extend_from_slice(v.as_bytes());
                        buffer.extend_from_slice(b"\r\n");
                    }
                    
                    buffer.to_vec()
                },
                DataType::Object { .. } => {
                    error!("DataType::Object response not fully implemented for Redis compatibility");
                    b"-ERR Object type not implemented\r\n".to_vec()
                },
                DataType::Nil => self.static_response(NIL_RESPONSE),
                DataType::Integer(i) => {
                    trace!("Wrapping Integer response: {}", i);
                    let i_str = i.to_string();
                    let buffer_size = 1 + i_str.len().to_string().len() + 2 + i_str.len() + 2;
                    let mut buffer = Self::allocate_buffer(buffer_size);
                    
                    buffer.put_u8(b'$');
                    buffer.extend_from_slice(i_str.len().to_string().as_bytes());
                    buffer.extend_from_slice(b"\r\n");
                    buffer.extend_from_slice(i_str.as_bytes());
                    buffer.extend_from_slice(b"\r\n");
                    
                    buffer.to_vec()
                },

                DataType::Document(value) => {
                    // Pre-allocate with generous buffer for JSON serialization
                    let mut buffer = Self::allocate_buffer(4096);
                    
                    match serde_json::to_vec(&value) {
                        Ok(json_bytes) => {
                            buffer.put_u8(b'$');
                            buffer.extend_from_slice(json_bytes.len().to_string().as_bytes());
                            buffer.extend_from_slice(b"\r\n");
                            buffer.extend_from_slice(&json_bytes);
                            buffer.extend_from_slice(b"\r\n");
                        }
                        Err(e) => {
                            error!("Serialization error for DataType::Document: {}", e);
                            buffer = Self::allocate_buffer(30);
                            buffer.extend_from_slice(b"-ERR serialization error\r\n");
                        }
                    }
                    
                    buffer.to_vec()
                },
                DataType::Graph(graph_data) => {
                    // Pre-allocate with generous buffer for Graph JSON serialization
                    let mut buffer = Self::allocate_buffer(8192);
                    
                    match serde_json::to_vec(&graph_data) {
                        Ok(json_bytes) => {
                            buffer.put_u8(b'$');
                            buffer.extend_from_slice(json_bytes.len().to_string().as_bytes());
                            buffer.extend_from_slice(b"\r\n");
                            buffer.extend_from_slice(&json_bytes);
                            buffer.extend_from_slice(b"\r\n");
                        },
                        Err(e) => {
                            error!("Serialization error for DataType::Graph: {}", e);
                            buffer = Self::allocate_buffer(35);
                            buffer.extend_from_slice(b"-ERR graph serialization error\r\n");
                        }
                    }
                    
                    buffer.to_vec()
                },
            },
            Err(e) => {
                error!("Error occurred: {:?}", e);
                let err_str = format!("-ERR {}\r\n", e);
                let mut buffer = Self::allocate_buffer(err_str.len());
                buffer.extend_from_slice(err_str.as_bytes());
                buffer.to_vec()
            }
        }
    }
    
    #[instrument(level = "trace", skip(self, cmd, active_connections, current_connection_arc), ret)]
    pub async fn execute(
        &self,
        cmd: Command,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
        current_connection_arc: Arc<Mutex<Connection>>
    ) -> Vec<u8> {
        match &cmd {
            Command::Ping {} => return self.static_response(PONG_RESPONSE),
            Command::Echo { message } => return self.wrap_response(Ok(DataType::String(message.clone()))),
            Command::CommandList {} => {
                return self.command_response_cache.get("COMMAND_LIST")
                    .cloned()
                    .unwrap_or_else(|| self.wrap_response(Ok(DataType::List(Self::get_command_list_resp()))))
            },
            Command::Command {} => {
                return self.command_response_cache.get("COMMAND")
                    .cloned()
                    .unwrap_or_else(|| self.wrap_response(Ok(DataType::List(Self::get_command_resp()))))
            },
            Command::Client { subcommand } => {
                return self.handle_client_command(subcommand.clone(), active_connections, current_connection_arc).await;
            },
            Command::Auth {  } => todo!(),
            _ => {}
        }
        
        info!("Executing command: {:?}", cmd);
        
        let result = match cmd {
            Command::Ping {} | Command::Echo { .. } | Command::CommandList {} | Command::Command {} | Command::Client { .. } | Command::Auth {  } => {
                // Already handled above
                unreachable!()
            },
            Command::CommandHelp { command_name } => Ok(DataType::List(Self::get_command_help_resp(&command_name))),
            Command::CommandInfo { command_name } => Ok(DataType::List(Self::get_command_info_resp(&command_name))),

            Command::Type { key } => self.store.type_cmd(&key).await,
            Command::Keys { pattern } => self.store.keys_cmd(&pattern).await,
            Command::Exists { key } => self.store.exists_cmd(&key).await,
            Command::Expire { key, seconds } =>  self.store.expire_cmd(&key, seconds).await,
            Command::Ttl { key } => self.store.ttl_cmd(&key).await,
            Command::Persist { key } => self.store.persist_cmd(&key).await,
            Command::Del { key } => self.store.del_cmd(&key).await,

            Command::Set { key, value, expire_option, set_option } => {
                match self.store.set_cmd(key, value, expire_option, set_option).await {
                    Ok(_) => Ok(DataType::String("OK".to_string())), 
                    Err(e) => Err(e)
                }
            }
            Command::Get { key } => self.store.get_cmd(&key).await,
            Command::Append { key, value } => self.store.append_cmd(&key, value).await,
            Command::StrLen { key } => self.store.strlen_cmd(&key).await,
            Command::GetRange { key, start, end } => self.store.getrange_cmd(&key, start, end).await,
            Command::SetRange { key, offset, value } => self.store.setrange_cmd(&key, offset, value).await,
            Command::GetSet { key, value } => self.store.getset_cmd(&key, value).await,
            Command::MGet { keys } => self.store.mget_cmd(&keys).await,
            Command::MSet { key_values } => {
                match self.store.mset_cmd(key_values).await {
                    Ok(_) => Ok(DataType::String("OK".to_string())),
                    Err(e) => Err(e)
                }
            }
            Command::Incr { key } => self.store.incr_cmd(&key).await,
            Command::Decr { key } => self.store.decr_cmd(&key).await,
            Command::IncrBy { key, increment } => self.store.incrby_cmd(&key, increment).await,
            Command::DecrBy { key, decrement } => self.store.decrby_cmd(&key, decrement).await,
            Command::IncrByFloat { key, increment } => self.store.incrbyfloat_cmd(&key, increment).await,

            Command::HSet { key, field_values } => self.store.hset_cmd(&key, field_values).await,
            Command::HGet { key, field } => self.store.hget_cmd(&key, field).await,
            Command::HDel { key, fields } => self.store.hdel_cmd(&key, fields).await,
            Command::HExists { key, field } => self.store.hexists_cmd(&key, field).await,
            Command::HGetAll { key } => self.store.hgetall_cmd(&key).await,
            Command::HIncrBy { key, field, increment } => self.store.hincrby_cmd(&key, field, increment).await,
            Command::HIncrByFloat { key, key_field_increment } => self.store.hincrbyfloat_cmd(&key, key_field_increment).await,
            Command::HKeys { key } => self.store.hkeys_cmd(&key).await,
            Command::HLen { key } => self.store.hlen_cmd(&key).await,
            Command::HMGet { key, fields } => self.store.hmget_cmd(&key, fields).await,
            Command::HMSet { key, field_values } => {
                match self.store.hmset_cmd(&key, field_values).await {
                    Ok(_) => Ok(DataType::String("OK".to_string())),
                    Err(e) => Err(e)
                }
            },
            Command::HSetNx { key, field, value } => self.store.hsetnx_cmd(&key, field, value).await,
            Command::HVals { key } => self.store.hvals_cmd(&key).await,

            Command::ListLPush { key, values } => self.store.lpush_cmd(key, values).await,
            Command::ListRPush { key, values } => self.store.rpush_cmd(key, values).await,
            Command::LPop { key } => self.store.lpop_cmd(&key).await,
            Command::RPop { key } => self.store.rpop_cmd(&key).await,
            Command::LLen { key } => self.store.llen_cmd(&key).await,
            Command::LRange { key, start, end } => self.store.lrange_cmd(&key, start, end).await,
            Command::LIndex { key, index } => self.store.lindex_cmd(&key, index).await,
            Command::LInsert { key, before_after, pivot, value } => self.store.linsert_cmd(&key, before_after, pivot, value).await,
            Command::LSet { key, index, value } => self.store.lset_cmd(&key, index, value).await,
            Command::LTrim { key, start, end } => self.store.ltrim_cmd(&key, start, end).await,
            Command::LRem { key, count, value } => self.store.lrem_cmd(&key, count, value).await,
            Command::RPopLPush { source, destination } => self.store.rpoplpush_cmd(&source, &destination).await,
            Command::BLPop { keys, timeout } => self.store.blpop_cmd(keys, timeout).await,
            Command::BRPop { keys, timeout } => self.store.brpop_cmd(keys, timeout).await,
            
            
            Command::JsonSet { key, path, value } => self.store.json_set_cmd(key, path, value).await,
            Command::JsonGet { key, path } => self.store.json_get_cmd(key, path).await,
            Command::JsonDel { key, path } => self.store.json_del_cmd(key, path).await,
            Command::JsonType { key, path } => self.store.json_type_cmd(key, path).await,
            Command::JsonNumIncrBy { key, path, increment } => self.store.json_numincrby_cmd(key, path, increment).await,
            Command::JsonStrAppend { key, path, value } => self.store.json_strappend_cmd(key, path, value).await,
            Command::JsonArrAppend { key, path, values } => self.store.json_arrappend_cmd(key, path, values).await,
            Command::JsonObjSet { key, path, key_to_set, value } => self.store.json_objset_cmd(key, path, key_to_set, value).await,
            Command::JsonObjKeys { key, path } => self.store.json_objkeys_cmd(key, path).await,
            Command::JsonObjLen { key, path } => self.store.json_objlen_cmd(key, path).await,
            Command::JsonArrIndex { key, path, value, range } => self.store.json_arrindex_cmd(key, path, value, range).await,
            Command::JsonArrInsert { key, path, index, values } => self.store.json_arrinsert_cmd(key, path, index, values).await,
            Command::JsonArrLen { key, path } => self.store.json_arrlen_cmd(key, path).await,
            Command::JsonArrPop { key, path, index } => self.store.json_arrpop_cmd(key, path, index).await,
            Command::JsonArrTrim { key, path, start, stop } => self.store.json_arrtrim_cmd(key, path, start, stop).await,


            Command::GraphCreateNode { key, node_id, properties } => {
                warn!("Graph commands not yet implemented");
                Err(CacheError::CommandNotImplemented("Graph commands not yet implemented".to_string()))
            },
            Command::GraphGetNode { key, node_id } => {
                Err(CacheError::CommandNotImplemented("Graph commands not yet implemented".to_string()))
            },
            Command::GraphDeleteNode { key, node_id } => {
                Err(CacheError::CommandNotImplemented("Graph commands not yet implemented".to_string()))
            },
            Command::GraphCreateEdge { key, edge_id, source_node_id, target_node_id, relation_type, properties } => {
                Err(CacheError::CommandNotImplemented("Graph commands not yet implemented".to_string()))
            },
            Command::GraphGetEdge { key, edge_id } => {
                Err(CacheError::CommandNotImplemented("Graph commands not yet implemented".to_string()))
            },
            Command::GraphDeleteEdge { key, edge_id } => {
                Err(CacheError::CommandNotImplemented("Graph commands not yet implemented".to_string()))
            },
            Command::GraphGetNodeProperties { key, node_id } => {
                Err(CacheError::CommandNotImplemented("Graph commands not yet implemented".to_string()))
            },
            Command::GraphSetNodeProperty { key, node_id, property_key, property_value } => {
                Err(CacheError::CommandNotImplemented("Graph commands not yet implemented".to_string()))
            },
            Command::GraphDeleteNodeProperty { key, node_id, property_key } => {
                Err(CacheError::CommandNotImplemented("Graph commands not yet implemented".to_string()))
            },
            Command::GraphGetEdgeProperties { key, edge_id } => {
                Err(CacheError::CommandNotImplemented("Graph commands not yet implemented".to_string()))
            },
            Command::GraphSetEdgeProperty { key, edge_id, property_key, property_value } => {
                Err(CacheError::CommandNotImplemented("Graph commands not yet implemented".to_string()))
            },
            Command::GraphDeleteEdgeProperty { key, edge_id, property_key } => {
                Err(CacheError::CommandNotImplemented("Graph commands not yet implemented".to_string()))
            },
            
            Command::Unknown(command_str) => {
                warn!("Unknown command received: {}", command_str);
                Err(CacheError::CommandNotImplemented(format!("Unknown command: {}", command_str)))
            },
            
        };

        self.wrap_response(result)
    }
    
    async fn handle_client_command(
        &self,
        subcommand: ClientSubCommand,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
        current_connection_arc: Arc<Mutex<Connection>>
    ) -> Vec<u8> {
        match subcommand {
            ClientSubCommand::List => {
                let client_list_info = self.get_client_list_info(active_connections).await;
                self.wrap_response(Ok(DataType::String(client_list_info)))
            },
            ClientSubCommand::Info => {
                let client_info = self.get_client_conn_info(current_connection_arc).await;
                self.wrap_response(Ok(DataType::String(client_info)))
            },
            ClientSubCommand::GetName => {
                let client_name = self.get_client_name(current_connection_arc).await;
                self.wrap_response(Ok(DataType::String(client_name)))
            },
            ClientSubCommand::SetName { connection_name } => {
                self.set_client_name(current_connection_arc, connection_name).await;
                self.static_response(OK_RESPONSE)
            },
            ClientSubCommand::Kill { ip_port } => {
                let kill_result = self.kill_client_connection(ip_port, active_connections).await;
                if kill_result {
                    self.static_response(OK_RESPONSE)
                } else {
                    let err_msg = "-ERR No client found matching address\r\n";
                    err_msg.as_bytes().to_vec()
                }
            },
            ClientSubCommand::NoSubCommand => {
                self.static_response(OK_RESPONSE)
            }
        }
    }

    
    /// Client list info generation
    pub async fn get_client_list_info(&self, active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>) -> String {

        let conn_count = active_connections.len();
        let estimated_size = conn_count * 128;
        let mut output = String::with_capacity(estimated_size);

        for entry in active_connections.iter() {
            if let Some(conn_arc) = entry.value().upgrade() {
                let client_info = {
                    let conn = conn_arc.lock().await;
                    let client_id = conn.get_client_id();
                    let peer_addr = conn.get_peer_address().ok();
                    let name = conn.get_client_name().await.unwrap_or_default();
                    (client_id, peer_addr, name)
                };

                // format the string
                if let Some(peer_addr) = client_info.1 {
                    let (client_id, _, name) = client_info;
                    output.push_str(&format!(
                        "id={} addr={} name={} age=0 idle=0 flags=N db=0 sub=0 psub=0 multi=- qbuf=0 qbuf-free=0 obl=0 oll=0 omem=0 events=r cmd=unknown client-type=normal\n",
                        client_id, peer_addr, name,
                    ));
                }
            }
        }
        output
    }

    /// Client connection info retrieval
    async fn get_client_conn_info(&self, current_connection_arc: Arc<Mutex<Connection>>) -> String {
        let client_data = {
            let conn = current_connection_arc.lock().await;
            let client_id = conn.get_client_id();
            let peer_addr = conn.get_peer_address().ok();
            let name = conn.get_client_name().await.unwrap_or_default();
            (client_id, peer_addr, name)
        };
        
        // Format response
        let mut output = String::with_capacity(256);
        if let Some(peer_addr) = client_data.1 {
            let (client_id, _, name) = client_data;
            output.push_str(&format!(
                "id:{}\r\naddr:{}\r\nname:{}\r\nage:0\r\nidle:0\r\nflags:N\r\ndb:0\r\nsub:0\r\npsub:0\r\nmulti:-1\r\nqbuf:0\r\nqbuf-free:0\r\nobl:0\r\noll:0\r\nomem:0\r\nevents:r\r\ncmd:unknown\r\nuser:default\r\nclient-type:normal\r\nname:{}\r\n",
                client_id, peer_addr, name, name,
            ));
        }
        output
    }

    /// Get client name with minimal locking
    pub async fn get_client_name(&self, current_connection_arc: Arc<Mutex<Connection>>) -> String {
        let conn = current_connection_arc.lock().await;
        conn.get_client_name().await.unwrap_or_default()
    }

    /// Set client name with minimal locking
    pub async fn set_client_name(&self, current_connection_arc: Arc<Mutex<Connection>>, connection_name: String) {
        let conn = current_connection_arc.lock().await;
        conn.set_client_name(connection_name).await;
    }

    /// Kill a client connection with efficient lookup
    pub async fn kill_client_connection(
        &self,
        ip_port_str: String,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>, RandomState>>,
    ) -> bool {
        // Fast path - parse address once outside the loop
        if let Ok((target_ip, target_port)) = parse_ip_port(&ip_port_str) {
            // Scan connections without fully acquiring each one
            for entry in active_connections.iter() {
                if let Some(conn_arc) = entry.value().upgrade() {
                    let should_kill = {
                        // Briefly lock just to get address
                        let conn = conn_arc.lock().await;
                        if let Ok(peer_addr) = conn.get_peer_address() {
                            peer_addr.ip() == target_ip && peer_addr.port() == target_port
                        } else {
                            false
                        }
                    };
                    
                    if should_kill {
                        info!("Killing client connection: {}", ip_port_str);
                        // The actual disconnection happens outside this function
                        // when a write fails or the connection is dropped
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Generate INFO command response according to Redis protocol
    pub async fn info(&self, section: Option<&str>) -> Vec<u8> {
        // Pre-allocate buffer with reasonable size
        let mut report = String::with_capacity(4096);
        
        // Return minimal necessary information for Redis compatibility
        report.push_str("# Server\r\n");
        report.push_str("calod_version:1.0.0\r\n");
        report.push_str("redis_mode:standalone\r\n");
        report.push_str("os:unknown\r\n");
        report.push_str("arch_bits:64\r\n");
        report.push_str("process_id:0\r\n");

        report.push_str("# Clients\r\n");
        report.push_str("connected_clients:1\r\n");
        report.push_str("client_recent_max_input_buffer:0\r\n");
        report.push_str("client_recent_max_output_buffer:0\r\n");
        report.push_str("blocked_clients:0\r\n");

        report.push_str("# Memory\r\n");
        report.push_str("used_memory:0\r\n");
        report.push_str("used_memory_human:0B\r\n");
        report.push_str("used_memory_peak:0\r\n");
        report.push_str("used_memory_peak_human:0B\r\n");
        report.push_str("used_memory_dataset:0\r\n");
        report.push_str("used_memory_dataset_human:0B\r\n");
        
        report.push_str("# Stats\r\n");
        report.push_str(&self.store.metrics());
        report.push_str("total_connections_received:1\r\n");
        report.push_str("total_commands_processed:0\r\n");
        
        report.push_str("# Replication\r\n");
        report.push_str("role:master\r\n");
        report.push_str("connected_slaves:0\r\n");
        report.push_str("master_replid:8371b4fb1155914e2fd21aa0e9adc5c0d821e821\r\n");
        report.push_str("master_replid2:0000000000000000000000000000000000000000\r\n");
        report.push_str("master_repl_offset:0\r\n");
        report.push_str("second_repl_offset:-1\r\n");
        report.push_str("repl_backlog_active:0\r\n");
        report.push_str("repl_backlog_size:1048576\r\n");
        report.push_str("repl_backlog_first_byte_offset:0\r\n");
        report.push_str("repl_backlog_histlen:0\r\n");
        
        // Include section-specific data if requested
        if let Some(sec) = section {
            report.push_str(&format!("# Section: {}\r\n", sec));
            match sec.to_lowercase().as_str() {
                "server" => {
                    report.push_str("tcp_port:6379\r\n");
                    report.push_str("uptime_in_seconds:0\r\n");
                    report.push_str("uptime_in_days:0\r\n");
                    report.push_str("hz:10\r\n");
                    report.push_str("executable:/path/to/calod\r\n");
                    report.push_str("config_file:\r\n");
                },
                "clients" => {
                    // Additional client info already provided above
                },
                "memory" => {
                    report.push_str("allocator:jemalloc\r\n");
                    report.push_str("active_defrag_running:0\r\n");
                    report.push_str("lazyfree_pending_objects:0\r\n");
                },
                "persistence" => {
                    report.push_str("loading:0\r\n");
                    report.push_str("rdb_changes_since_last_save:0\r\n");
                    report.push_str("rdb_bgsave_in_progress:0\r\n");
                    report.push_str("rdb_last_save_time:0\r\n");
                    report.push_str("aof_enabled:0\r\n");
                    report.push_str("aof_rewrite_in_progress:0\r\n");
                    report.push_str("aof_rewrite_scheduled:0\r\n");
                },
                "stats" => {
                    report.push_str("instantaneous_ops_per_sec:0\r\n");
                    report.push_str("instantaneous_input_kbps:0.00\r\n");
                    report.push_str("instantaneous_output_kbps:0.00\r\n");
                    report.push_str("rejected_connections:0\r\n");
                    report.push_str("expired_keys:0\r\n");
                    report.push_str("evicted_keys:0\r\n");
                    report.push_str("keyspace_hits:0\r\n");
                    report.push_str("keyspace_misses:0\r\n");
                },
                "replication" => {
                    // Additional replication info already provided above
                },
                "cpu" => {
                    report.push_str("used_cpu_sys:0.0\r\n");
                    report.push_str("used_cpu_user:0.0\r\n");
                    report.push_str("used_cpu_sys_children:0.0\r\n");
                    report.push_str("used_cpu_user_children:0.0\r\n");
                },
                "commandstats" => {
                    report.push_str("cmdstat_ping:calls=0,usec=0,usec_per_call=0.00\r\n");
                    report.push_str("cmdstat_get:calls=0,usec=0,usec_per_call=0.00\r\n");
                    report.push_str("cmdstat_set:calls=0,usec=0,usec_per_call=0.00\r\n");
                },
                "cluster" => {
                    report.push_str("cluster_enabled:0\r\n");
                },
                "keyspace" => {
                    report.push_str("db0:keys=0,expires=0,avg_ttl=0\r\n");
                },
                _ => {
                    // No special handling for unknown sections
                }
            }
        }

        // Format the response according to Redis RESP protocol for bulk strings
        let resp = format!("${}\r\n{}\r\n", report.len(), report);
        debug!("INFO response generated, section: {:?}", section);
        resp.into_bytes()
    }

    /// Precompute COMMAND response for better performance
    fn precompute_command_resp() -> Vec<u8> {
        let commands = Self::get_command_resp();
        let mut buffer = BytesMut::with_capacity(commands.len() * 20); // Reasonable estimate
        
        // Format as Redis array
        buffer.put_u8(b'*');
        buffer.extend_from_slice(commands.len().to_string().as_bytes());
        buffer.extend_from_slice(b"\r\n");
        
        // Add each command entry
        for cmd in &commands {
            buffer.put_u8(b'$');
            buffer.extend_from_slice(cmd.len().to_string().as_bytes());
            buffer.extend_from_slice(b"\r\n");
            buffer.extend_from_slice(cmd.as_bytes());
            buffer.extend_from_slice(b"\r\n");
        }
        
        buffer.to_vec()
    }
    
    /// Precompute COMMAND LIST response
    fn precompute_command_list_resp() -> Vec<u8> {
        let commands = Self::get_command_list_resp();
        let mut buffer = BytesMut::with_capacity(commands.len() * 15); // Reasonable estimate
        
        // Format as Redis array
        buffer.put_u8(b'*');
        buffer.extend_from_slice(commands.len().to_string().as_bytes());
        buffer.extend_from_slice(b"\r\n");
        
        // Add each command name
        for cmd in &commands {
            buffer.put_u8(b'$');
            buffer.extend_from_slice(cmd.len().to_string().as_bytes());
            buffer.extend_from_slice(b"\r\n");
            buffer.extend_from_slice(cmd.as_bytes());
            buffer.extend_from_slice(b"\r\n");
        }
        
        buffer.to_vec()
    }


    // Redis compatibility: COMMAND response format
    fn get_command_resp() -> Vec<String> {
        // Return RESP array for COMMAND command (list of all commands and arities)
        // Format matches Redis command info structure
        Vec::<String>::from([
            String::from("PING"),
            String::from("-1"), // Arity -1 for variable arguments or -N for commands taking exactly N arguments
            String::from("readonly,fast"), // Command flags for Redis compatibility
            String::from("0"), // First key position (0-based or -1 if none)
            String::from("0"), // Last key position
            String::from("1"), // Step count for locating keys
            
            String::from("ECHO"),
            String::from("2"), // Arity 2 (including command name and one argument)
            String::from("readonly,fast"),
            String::from("-1"), // No key
            String::from("-1"),
            String::from("0"), 
            
            String::from("SET"),
            String::from("-3"), // Arity -3 (including command name, key, value, and optional expire/NX/XX)
            String::from("write,denyoom"),
            String::from("1"), // Key at position 1
            String::from("1"), 
            String::from("1"),
            
            String::from("GET"),
            String::from("2"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("EXISTS"),
            String::from("2"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("DEL"),
            String::from("2"),
            String::from("write"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("KEYS"),
            String::from("2"),
            String::from("readonly,sort_for_script"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            
            String::from("EXPIRE"),
            String::from("3"),
            String::from("write"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("TTL"),
            String::from("2"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("PERSIST"),
            String::from("2"),
            String::from("write"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("INCR"),
            String::from("2"),
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("DECR"),
            String::from("2"),
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("INCRBY"),
            String::from("3"),
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("DECRBY"),
            String::from("3"),
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("INCRBYFLOAT"),
            String::from("3"),
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            // Hash commands
            String::from("HSET"),
            String::from("-3"), // Variable arity for HSET key field value [field value ...]
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("HGET"),
            String::from("3"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("HDEL"),
            String::from("-3"), // Variable arity for HDEL key field [field ...]
            String::from("write,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("HEXISTS"),
            String::from("3"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("HGETALL"),
            String::from("2"),
            String::from("readonly"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            // List commands
            String::from("LPUSH"),
            String::from("-3"), // Variable arity for LPUSH key value [value ...]
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("RPUSH"),
            String::from("-3"), // Variable arity for RPUSH key value [value ...]
            String::from("write,denyoom,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("LPOP"),
            String::from("2"),
            String::from("write,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("RPOP"),
            String::from("2"),
            String::from("write,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("BLPOP"),
            String::from("-3"), // Variable arity for BLPOP key [key ...] timeout
            String::from("write,noscript,blocking"),
            String::from("1"),
            String::from("-2"),
            String::from("1"),
            
            String::from("BRPOP"),
            String::from("-3"), // Variable arity for BRPOP key [key ...] timeout
            String::from("write,noscript,blocking"),
            String::from("1"),
            String::from("-2"),
            String::from("1"),
            
            // JSON commands with Redis-like metadata
            String::from("JSON.SET"),
            String::from("4"),
            String::from("write,denyoom"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            String::from("JSON.GET"),
            String::from("3"),
            String::from("readonly,fast"),
            String::from("1"),
            String::from("1"),
            String::from("1"),
            
            // Admin commands
            String::from("COMMAND"),
            String::from("-1"), // Arity -1 for variable arguments
            String::from("readonly,loading,stale"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            
            String::from("INFO"),
            String::from("-1"), // Arity -1 for variable arguments
            String::from("readonly,loading,stale"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
            
            String::from("CLIENT"),
            String::from("-2"), // Arity -2 for CLIENT subcommands
            String::from("admin,noscript,loading,stale"),
            String::from("0"),
            String::from("0"),
            String::from("0"),
        ])
    }

    fn get_command_list_resp() -> Vec<String> {
        // Return RESP array for COMMAND LIST (list of command names)
        // Redis clients expect lowercase command names
        Vec::<String>::from([
            String::from("ping"),
            String::from("echo"),
            String::from("set"),
            String::from("get"),
            String::from("exists"),
            String::from("del"),
            String::from("keys"),
            String::from("expire"),
            String::from("ttl"),
            String::from("persist"),
            String::from("incr"),
            String::from("decr"),
            String::from("incrby"),
            String::from("decrby"),
            String::from("incrbyfloat"),
            String::from("hset"),
            String::from("hget"),
            String::from("hdel"),
            String::from("hexists"),
            String::from("hgetall"),
            String::from("hincrby"),
            String::from("hincrbyfloat"),
            String::from("hkeys"),
            String::from("hlen"),
            String::from("hmget"),
            String::from("hmset"),
            String::from("hsetnx"),
            String::from("hvals"),
            String::from("lpush"),
            String::from("rpush"),
            String::from("lpop"),
            String::from("rpop"),
            String::from("llen"),
            String::from("lrange"),
            String::from("lindex"),
            String::from("linsert"),
            String::from("lset"),
            String::from("ltrim"),
            String::from("lrem"),
            String::from("rpoplpush"),
            String::from("blpop"),
            String::from("brpop"),
            String::from("type"),
            String::from("json.set"),
            String::from("json.get"),
            String::from("json.del"),
            String::from("json.type"),
            String::from("json.numincrby"),
            String::from("json.arrappend"),
            String::from("json.arrlen"),
            String::from("command"),
            String::from("info"),
            String::from("client"),
        ])
    }

    fn get_command_info_resp(command_name: &str) -> Vec<String> {
        // Return RESP array for COMMAND INFO <command_name> (command details)
        match command_name.to_uppercase().as_str() {
            "PING" => Vec::<String>::from([
                String::from("name"), String::from("ping"),
                String::from("arity"), String::from("1"),
                String::from("flags"), String::from("readonly,fast"), // Example flags
                String::from("firstkey"), String::from("0"),
                String::from("lastkey"), String::from("0"),
                String::from("steps"), String::from("1"),
            ]),
            "SET" => Vec::<String>::from([
                String::from("name"), String::from("set"),
                String::from("arity"), String::from("-3"),
                String::from("flags"), String::from("write,denyoom"),
                String::from("firstkey"), String::from("1"),
                String::from("lastkey"), String::from("1"),
                String::from("steps"), String::from("1"),
            ]),
            "GET" => Vec::<String>::from([
                String::from("name"), String::from("get"),
                String::from("arity"), String::from("2"),
                String::from("flags"), String::from("readonly,fast"),
                String::from("firstkey"), String::from("1"),
                String::from("lastkey"), String::from("1"),
                String::from("steps"), String::from("1"),
            ]),
            "COMMAND" => Vec::<String>::from([
                String::from("name"), String::from("command"),
                String::from("arity"), String::from("-1"),
                String::from("flags"), String::from("readonly,admin,noscript,loading,stale"), // Example flags
                String::from("firstkey"), String::from("0"),
                String::from("lastkey"), String::from("0"),
                String::from("steps"), String::from("1"),
            ]),
            "INFO" => Vec::<String>::from([
                String::from("name"), String::from("info"),
                String::from("arity"), String::from("-1"),
                String::from("flags"), String::from("readonly,loading,stale"), // Example flags
                String::from("firstkey"), String::from("0"),
                String::from("lastkey"), String::from("0"),
                String::from("steps"), String::from("1"),
            ]),
            _ => Vec::<String>::from([ // Default info for unknown commands
                String::from("name"), String::from(command_name.to_lowercase()),
                String::from("arity"), String::from("0"),
                String::from("flags"), String::from("unknown"),
                String::from("firstkey"), String::from("0"),
                String::from("lastkey"), String::from("0"),
                String::from("steps"), String::from("1"),
            ])
        }
    }

    fn get_command_help_resp(command_name: &str) -> Vec<String> {
        // Return RESP array for COMMAND HELP <command_name> (help text)
        // Format compatible with Redis clients
        match command_name.to_uppercase().as_str() {
            "PING" => Vec::<String>::from([
                String::from("PING [message]"),
                String::from("Summary: Ping the server"),
                String::from("Since: 1.0.0"),
                String::from("Group: connection")
            ]),
            "SET" => Vec::<String>::from([
                String::from("SET key value [EX seconds|PX milliseconds] [NX|XX]"),
                String::from("Summary: Set the string value of a key"),
                String::from("Since: 1.0.0"),
                String::from("Group: string")
            ]),
            "GET" => Vec::<String>::from([
                String::from("GET key"),
                String::from("Summary: Get the value of a key"),
                String::from("Since: 1.0.0"),
                String::from("Group: string")
            ]),
            "COMMAND" => Vec::<String>::from([
                String::from("COMMAND [subcommand [arg [arg ...]]]"),
                String::from("Summary: Get array of Redis command details"),
                String::from("Since: 2.8.13"),
                String::from("Group: server")
            ]),
            "INFO" => Vec::<String>::from([
                String::from("INFO [section]"),
                String::from("Summary: Get information and statistics about the server"),
                String::from("Since: 1.0.0"),
                String::from("Group: server")
            ]),
            _ => Vec::<String>::from([
                String::from(format!("{} (no additional help available)", command_name.to_uppercase())),
                String::from("Summary: Command help not available"),
                String::from("Group: unknown")
            ])
        }
    }
}