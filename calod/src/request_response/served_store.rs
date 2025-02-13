use std::{net::IpAddr, sync::{Arc, Weak}};

use bytes::BytesMut;
use dashmap::DashMap;
use tokio::sync::Mutex;
use tracing::{debug, error, info, instrument, trace, warn};
use uuid::Uuid;

use crate::store::{calod_store::{CacheError, ShardedStore}, config::CacheConfig};
use crate::store::calod_data::DataType;
use super::{command::{ClientSubCommand, Command}, server::Connection};

type ClientId = Uuid;

pub struct ServedCalodStore {
    store: Arc<ShardedStore>,
}

fn parse_ip_port(ip_port_str: &str) -> Result<(IpAddr, u16), String> {
    let parts: Vec<&str> = ip_port_str.split(':').collect();
    if parts.len() != 2 {
        return Err("Invalid ip:port format".to_string());
    }
    let ip_str = parts[0];
    let port_str = parts[1];

    let ip_addr: IpAddr = ip_str.parse().map_err(|_| "Invalid IP address".to_string())?;
    let port: u16 = port_str.parse().map_err(|_| "Invalid port number".to_string())?;

    Ok((ip_addr, port))
}

impl ServedCalodStore {
    pub fn new() -> Self {
        let config = CacheConfig {
            shards: 64,
            capacity: 10_000_000,
            metrics_enabled: true,
            ..Default::default()
        };
        
        ServedCalodStore {
            store: Arc::new(ShardedStore::new(config)),
        }
    }

    fn wrap_simple_response(&self, status: &str) -> Vec<u8> {
        format!("+{}\r\n", status).into_bytes()
    }
    
    fn wrap_response(&self, data: Result<DataType, CacheError>) -> Vec<u8> {
        let mut buffer = BytesMut::new();

        match data {
            Ok(data_type) => match data_type {
                DataType::String(s) => {
                    trace!("Wrapping String response: {}", s);
                    buffer.extend_from_slice(format!("${}\r\n", s.len()).as_bytes());
                    buffer.extend_from_slice(s.as_bytes());
                    buffer.extend_from_slice(b"\r\n");
                },
                DataType::List(items) => {
                    trace!("Wrapping List response, {} items", items.len());
                    buffer.extend_from_slice(format!("*{}\r\n", items.len()).as_bytes());
                    for item in items {
                        buffer.extend_from_slice(format!("${}\r\n", item.len()).as_bytes());
                        buffer.extend_from_slice(item.as_bytes());
                        buffer.extend_from_slice(b"\r\n");
                    }
                }
                DataType::Set(items) => {
                    trace!("Wrapping Set response, {} items", items.len());
                    buffer.extend_from_slice(format!("*{}\r\n", items.len()).as_bytes());
                    for item in items {
                        buffer.extend_from_slice(format!("${}\r\n", item.len()).as_bytes());
                        buffer.extend_from_slice(item.as_bytes());
                        buffer.extend_from_slice(b"\r\n");
                    }
                },
                DataType::Hash(map) => {
                    trace!("Wrapping Hash response, {} entries", map.len());
                    buffer.extend_from_slice(format!("*{}\r\n", map.len() * 2).as_bytes());
                    for (k, v) in map {
                        buffer.extend_from_slice(format!("${}\r\n", k.len()).as_bytes());
                        buffer.extend_from_slice(k.as_bytes());
                        buffer.extend_from_slice(b"\r\n");
                        buffer.extend_from_slice(format!("${}\r\n", v.len()).as_bytes());
                        buffer.extend_from_slice(v.as_bytes());
                        buffer.extend_from_slice(b"\r\n");
                    }
                },
                DataType::Object { .. } => {
                    error!("DataType::Object response not fully implemented for Redis compatibility");
                    buffer.extend_from_slice(b"-ERR Object type not implemented\r\n");
                },
                DataType::Nil => {
                    trace!("Wrapping Nil response");
                    buffer.extend_from_slice(b"$-1\r\n")
                },
                DataType::Integer(i) => {
                    trace!("Wrapping Integer response: {}", i);
                    buffer.extend_from_slice(format!(":{}\r\n", i).as_bytes())
                },

                DataType::Document(value) => {
                    match serde_json::to_string(&value) {
                        Ok(json_string) => {
                            buffer.extend_from_slice(format!("${}\r\n", json_string.len()).as_bytes());
                            buffer.extend_from_slice(json_string.as_bytes());
                            buffer.extend_from_slice(b"\r\n");
                        }
                        Err(e) => {
                            error!("Serialization error for DataType::Document: {}", e);
                            buffer.extend_from_slice(b"-ERR serialization error\r\n");
                        }
                    }
                },
                DataType::Graph(graph_data) => {
                    match serde_json::to_string(&graph_data) {
                        Ok(json_string) => {
                            buffer.extend_from_slice(format!("${}\r\n", json_string.len()).as_bytes());
                            buffer.extend_from_slice(json_string.as_bytes());
                            buffer.extend_from_slice(b"\r\n");
                        },
                        Err(e) => {
                            error!("Serialization error for DataType::Graph: {}", e);
                            buffer.extend_from_slice(b"-ERR graph serialization error\r\n");
                        }
                    }
                },
            },
            Err(e) => {
                error!("Error occured : {:?}", e);
                buffer.extend_from_slice(format!("-ERR {}\r\n", e).as_bytes());
            }
        }

        buffer.to_vec()
    }
    
    #[instrument(level = "trace", skip(self, cmd, active_connections, current_connection_arc), ret)]
    pub async fn execute(
        &self,
        cmd: Command,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>>>,
        current_connection_arc: Arc<Mutex<Connection>>
    ) -> Vec<u8> {
        info!("Executing command: {:?}", cmd);
        
        let result = match cmd {
            Command::Ping {} => Ok(DataType::String(self.store.ping().await)),
            Command::Echo { message } => Ok(DataType::String(message)),
            Command::CommandList {} => Ok(DataType::List(Self::get_command_list_resp())),
            Command::CommandHelp { command_name } => Ok(DataType::List(Self::get_command_help_resp(&command_name))),
            Command::CommandInfo { command_name } => Ok(DataType::List(Self::get_command_info_resp(&command_name))),
            Command::Command {} => Ok(DataType::List(Self::get_command_resp())),
            Command::Client { subcommand } => {
                return self.handle_client_command(subcommand, active_connections, current_connection_arc).await;
            },

            Command::Type { key } => self.store.type_cmd(&key).await,
            Command::Keys { pattern } => self.store.keys_cmd(&pattern).await,
            Command::Exists { key } => self.store.exists_cmd(&key).await,
            Command::Expire { key, seconds } =>  self.store.expire_cmd(&key, seconds).await,
            Command::Ttl { key } => self.store.ttl_cmd(&key).await,
            Command::Persist { key } => self.store.persist_cmd(&key).await,
            Command::Del { key } => self.store.del_cmd(&key).await,

            Command::Set { key, value, expire_option, set_option } => {
                self.store.set_cmd(key, value, expire_option, set_option).await.map(|_| DataType::String("OK".to_string())) // SET returns +OK, map to DataType::String("OK")
            }
            Command::Get { key } => self.store.get_cmd(&key).await,
            Command::Append { key, value } => self.store.append_cmd(&key, value).await,
            Command::StrLen { key } => self.store.strlen_cmd(&key).await,
            Command::GetRange { key, start, end } => self.store.getrange_cmd(&key, start, end).await,
            Command::SetRange { key, offset, value } => self.store.setrange_cmd(&key, offset, value).await,
            Command::GetSet { key, value } => self.store.getset_cmd(&key, value).await,
            Command::MGet { keys } => self.store.mget_cmd(&keys).await,
            Command::MSet { key_values } => {
                self.store.mset_cmd(key_values).await.map(|_| DataType::String("OK".to_string())) // MSET returns +OK
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
            Command::HMSet { key, field_values } => self.store.hmset_cmd(&key, field_values).await.map(|_| DataType::String("OK".to_string())), // HMSET returns +OK
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


            Command::GraphCreateNode { key, node_id, properties } => todo!(),
            Command::GraphGetNode { key, node_id } => todo!(),
            Command::GraphDeleteNode { key, node_id } => todo!(),
            Command::GraphCreateEdge { key, edge_id, source_node_id, target_node_id, relation_type, properties } => todo!(),
            Command::GraphGetEdge { key, edge_id } => todo!(),
            Command::GraphDeleteEdge { key, edge_id } => todo!(),
            Command::GraphGetNodeProperties { key, node_id } => todo!(),
            Command::GraphSetNodeProperty { key, node_id, property_key, property_value } => todo!(),
            Command::GraphDeleteNodeProperty { key, node_id, property_key } => todo!(),
            Command::GraphGetEdgeProperties { key, edge_id } => todo!(),
            Command::GraphSetEdgeProperty { key, edge_id, property_key, property_value } => todo!(),
            Command::GraphDeleteEdgeProperty { key, edge_id, property_key } => todo!(),
            
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
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>>>,
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
                self.wrap_simple_response("OK")
            },
            ClientSubCommand::Kill { ip_port } => {
                let kill_result = self.kill_client_connection(ip_port, active_connections).await;
                if kill_result {
                    self.wrap_simple_response("OK")
                } else {
                    self.wrap_response(Err(CacheError::InvalidCommandArguments("No client found matching address".to_string())))
                }
            },
            ClientSubCommand::NoSubCommand => {
                self.wrap_response(Err(CacheError::CommandNotImplemented("CLIENT command requires a subcommand (LIST, INFO, GETNAME, SETNAME, KILL)".to_string())))
            }
        }
    }

    
    pub async fn get_client_list_info(&self, active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>>>) -> String {
        let mut output = String::new();
        for entry in active_connections.iter() {
            if let Some(conn_arc) = entry.value().upgrade() {
                let conn = conn_arc.lock().await;
                let client_id = conn.get_client_id();
                if let Ok(peer_addr) = conn.get_peer_address() {
                    let name = conn.get_client_name().await.unwrap_or_default();
                    output.push_str(&format!(
                        "id={} addr={} name={} age=0 idle=0 flags=N db=0 sub=0 psub=0 multi=- qbuf=0 qbuf-free=0 obl=0 oll=0 omem=0 events=r cmd=unknown client-type=normal\n",
                        client_id, peer_addr, name,
                    ));
                }
            }
        }
        output
    }

    async fn get_client_conn_info(&self, current_connection_arc: Arc<Mutex<Connection>>) -> String {
        let conn = current_connection_arc.lock().await;
        let client_id = conn.get_client_id();
        let mut output = String::new();
        if let Ok(peer_addr) = conn.get_peer_address() {
            let name = conn.get_client_name().await.unwrap_or_default();
            output.push_str(&format!(
                "id:{}\r\naddr:{}\r\nname:{}\r\nage:0\r\nidle:0\r\nflags:N\r\ndb:0\r\nsub:0\r\npsub:0\r\nmulti:-1\r\nqbuf:0\r\nqbuf-free:0\r\nobl:0\r\noll:0\r\nomem:0\r\nevents:r\r\ncmd:unknown\r\nuser:default\r\nclient-type:normal\r\nname:{}\r\n",
                client_id, peer_addr, name, name,
            ));
        }
        output
    }

    pub async fn get_client_name(&self, current_connection_arc: Arc<Mutex<Connection>>) -> String {
        let conn = current_connection_arc.lock().await;
        conn.get_client_name().await.unwrap_or_default()
    }

    pub async fn set_client_name(&self, current_connection_arc: Arc<Mutex<Connection>>, connection_name: String) {
        let conn = current_connection_arc.lock().await;
        conn.set_client_name(connection_name).await;
    }

    pub async fn kill_client_connection(
        &self,
        ip_port_str: String,
        active_connections: Arc<DashMap<ClientId, Weak<Mutex<Connection>>>>,
    ) -> bool {
        if let Ok((ip_addr, port)) = parse_ip_port(&ip_port_str) {
            for entry in active_connections.iter() {
                if let Some(conn_arc) = entry.value().upgrade() {
                    // Lock the connection asynchronously.
                    let conn = conn_arc.lock().await;
                    if let Ok(peer_addr) = conn.get_peer_address() {
                        if peer_addr.ip() == ip_addr && peer_addr.port() == port {
                            info!("Killing client connection: {}", peer_addr);
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    pub async fn info(&self, section: Option<&str>) -> Vec<u8> {
        let mut report = String::new();
        report.push_str(&format!("# Server\r\n"));
        report.push_str(&format!("calod_version:0.1.0\r\n"));

        report.push_str(&format!("# Stats\r\n"));
        report.push_str(&self.store.metrics());

        if let Some(sec) = section {
            report.push_str(&format!("# Section: {}\r\n", sec));
        }

        let response_bytes = format!("{}\r\n", report).into_bytes();
        debug!("INFO response generated, section: {:?}", section);
        response_bytes
    }

    fn get_command_resp() -> Vec<String> {
        // Return RESP array for COMMAND command (list of all commands and arities)
        Vec::<String>::from([
            String::from("PING"),
            String::from("-1"), // Arity -1 for variable arguments or -N for commands taking exactly N arguments
            // Add flags if needed, e.g., String::from("flags"), String::from("readonly,pubsub,...")
            String::from("ECHO"),
            String::from("2"), // Arity 2 (including command name and one argument)
            String::from("SET"),
            String::from("-3"), // Arity -3 (including command name, key, value, and optional expire/NX/XX)
            String::from("GET"),
            String::from("2"),
            String::from("EXISTS"),
            String::from("2"),
            String::from("DEL"),
            String::from("2"),
            String::from("KEYS"),
            String::from("2"),
            String::from("EXPIRE"),
            String::from("3"),
            String::from("TTL"),
            String::from("2"),
            String::from("PERSIST"),
            String::from("2"),
            String::from("INCR"),
            String::from("2"),
            String::from("DECR"),
            String::from("2"),
            String::from("INCRBY"),
            String::from("3"),
            String::from("DECRBY"),
            String::from("3"),
            String::from("INCRBYFLOAT"),
            String::from("3"),
            String::from("HSET"),
            String::from("-3"), // Variable arity for HSET key field value [field value ...]
            String::from("HGET"),
            String::from("3"),
            String::from("HDEL"),
            String::from("-3"), // Variable arity for HDEL key field [field ...]
            String::from("HEXISTS"),
            String::from("3"),
            String::from("HGETALL"),
            String::from("2"),
            String::from("HINCRBY"),
            String::from("4"),
            String::from("HINCRBYFLOAT"),
            String::from("4"),
            String::from("HKEYS"),
            String::from("2"),
            String::from("HLEN"),
            String::from("2"),
            String::from("HMGET"),
            String::from("-3"), // Variable arity for HMGET key field [field ...]
            String::from("HMSET"),
            String::from("-3"), // Variable arity for HMSET key field value [field value ...]
            String::from("HSETNX"),
            String::from("4"),
            String::from("HVALS"),
            String::from("2"),
            String::from("LPUSH"),
            String::from("-3"), // Variable arity for LPUSH key value [value ...]
            String::from("RPUSH"),
            String::from("-3"), // Variable arity for RPUSH key value [value ...]
            String::from("LPOP"),
            String::from("2"),
            String::from("RPOP"),
            String::from("2"),
            String::from("LLEN"),
            String::from("2"),
            String::from("LRANGE"),
            String::from("4"),
            String::from("LINDEX"),
            String::from("3"),
            String::from("LINSERT"),
            String::from("5"),
            String::from("LSET"),
            String::from("4"),
            String::from("LTRIM"),
            String::from("4"),
            String::from("LREM"),
            String::from("4"),
            String::from("RPOPLPUSH"),
            String::from("3"),
            String::from("BLPOP"),
            String::from("-3"), // Variable arity for BLPOP key [key ...] timeout
            String::from("BRPOP"),
            String::from("-3"), // Variable arity for BRPOP key [key ...] timeout
            String::from("TYPE"),
            String::from("2"),
            String::from("COMMAND"),
            String::from("-1"), // Arity -1 for variable arguments
            String::from("INFO"),
            String::from("-1"), // Arity -1 for variable arguments
        ])
    }

    fn get_command_list_resp() -> Vec<String> {
        // Return RESP array for COMMAND LIST (list of command names)
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
            String::from("command"),
            String::from("info"),
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
                String::from("name"), String::from(command_name),
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
        match command_name.to_uppercase().as_str() {
            "PING" => Vec::<String>::from([String::from("PING")]), // Placeholder help text
            "SET" => Vec::<String>::from([String::from("SET key value [EX seconds|PX milliseconds] [NX|XX]")]), // Placeholder help text
            "GET" => Vec::<String>::from([String::from("GET key")]), // Placeholder help text
            "COMMAND" => Vec::<String>::from([String::from("COMMAND")]), // Placeholder help text
            "INFO" => Vec::<String>::from([String::from("INFO [section]")]), // Placeholder help text
            _ => Vec::<String>::from([String::from("HELP for UNKNOWN command")]), // Placeholder for unknown commands
        }
    }
}