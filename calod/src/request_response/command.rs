use serde_json;
use tracing::{error, info};
use std::convert::TryFrom;
use thiserror::Error;

use crate::parser::parser::Value;

#[derive(Debug, Error)]
pub enum CommandError {
    #[error("Invalid command format")]
    InvalidFormat,
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),
    #[error("JSON parse error: {0}")]
    JsonError(#[from] serde_json::Error),
}

#[derive(Debug, PartialEq)]
pub enum SetExpireOption {
    EX(u64),  // EX seconds
    PX(u64),  // PX milliseconds
}

#[derive(Debug, PartialEq)]
pub enum SetOption {
    NX,       // NX -- Only set the key if it does not already exist.
    XX,       // XX -- Only set the key if it already exist.
}

#[derive(Debug, PartialEq)]
pub enum InsertOption {
    Before,
    After,
}

#[derive(Debug, PartialEq)]
pub enum ClientSubCommand {
    List,
    Info,
    GetName,
    SetName { connection_name: String },
    Kill { ip_port: String }, // or ClientKillFilter, for more complex KILL options
    NoSubCommand, // Represent no subcommand or invalid subcommand for CLIENT
}

#[derive(Debug, PartialEq)]
pub enum Command {
    // Connection commands
    // Generic
    Ping {},
    Echo { message: String },
    CommandInfo { command_name: String },
    CommandList {},
    CommandHelp { command_name: String },
    Command {},
    Client { subcommand: ClientSubCommand },

    // Generic commands
    Type { key: String },
    Keys { pattern: String },
    Exists { key: String },
    Expire { key: String, seconds: u64 },
    Ttl { key: String },
    Persist { key: String },
    Del { key: String },

    // Strings/Numbers commands
    Set { key: String, value: String, expire_option: Option<SetExpireOption>, set_option: Option<SetOption> },
    Get { key: String },
    Append { key: String, value: String },
    StrLen { key: String },
    GetRange { key: String, start: isize, end: isize },
    SetRange { key: String, offset: usize, value: String },
    GetSet { key: String, value: String },
    MGet { keys: Vec<String> }, // MGET key [key ...]
    MSet { key_values: Vec<(String, String)> }, // MSET key value [key value ...]
    Incr { key: String },
    Decr { key: String },
    IncrBy { key: String, increment: i64 },
    DecrBy { key: String, decrement: i64 },
    IncrByFloat { key: String, increment: f64 },

    // Hash commands
    HSet { key: String, field_values: Vec<(String, String)> }, // HSET key field value [field value ...]
    HGet { key: String, field: String },
    HDel { key: String, fields: Vec<String> }, // HDEL key field [field ...]
    HExists { key: String, field: String },
    HGetAll { key: String },
    HIncrBy { key: String, field: String, increment: i64 },
    HIncrByFloat { key: String, key_field_increment: Vec<(String, f64)> }, //HINCRBYFLOAT key field increment [field increment ...]
    HKeys { key: String },
    HLen { key: String },
    HMGet { key: String, fields: Vec<String> }, // HMGET key field [field ...]
    HMSet { key: String, field_values: Vec<(String, String)> }, // HMSET key field value [field value ...]
    HSetNx { key: String, field: String, value: String },
    HVals { key: String },
    
    // List Commands
    ListLPush { key: String, values: Vec<String> }, // LPUSH key value [value ...]
    ListRPush { key: String, values: Vec<String> }, // RPUSH key value [value ...]
    LPop { key: String },
    RPop { key: String },
    LLen { key: String },
    LRange { key: String, start: isize, end: isize },
    LIndex { key: String, index: isize },
    LInsert { key: String, before_after: InsertOption, pivot: String, value: String }, // LINSERT key BEFORE|AFTER pivot value
    LSet { key: String, index: isize, value: String },
    LTrim { key: String, start: isize, end: isize },
    LRem { key: String, count: i64, value: String },
    RPopLPush { source: String, destination: String },
    BLPop { keys: Vec<String>, timeout: f64 }, // BLPOP key [key ...] timeout
    BRPop { keys: Vec<String>, timeout: f64 }, // BRPOP key [key ...] timeout

    // JSON Commands
    JsonSet { key: String, path: String, value: String }, // JSON.SET key path value
    JsonGet { key: String, path: String },           // JSON.GET key [path]
    JsonDel { key: String, path: String },           // JSON.DEL key [path]
    JsonType { key: String, path: String },          // JSON.TYPE key [path]
    JsonNumIncrBy { key: String, path: String, increment: f64 }, // JSON.NUMINCRBY key path increment
    JsonStrAppend { key: String, path: String, value: String }, // JSON.STRAPPEND key path value
    JsonArrAppend { key: String, path: String, values: Vec<String> }, // JSON.ARRAPPEND key path value [value ...]
    JsonObjSet { key: String, path: String, key_to_set: String, value: String }, // JSON.OBJSET key path key value
    JsonObjKeys { key: String, path: String },        // JSON.OBJKEYS key [path]
    JsonObjLen { key: String, path: String },         // JSON.OBJLEN key [path]
    JsonArrIndex { key: String, path: String, value: String, range: Option<(isize, isize)> }, // JSON.ARRINDEX key path value [start [stop]]
    JsonArrInsert { key: String, path: String, index: isize, values: Vec<String> }, // JSON.ARRINSERT key path index value [value ...]
    JsonArrLen { key: String, path: String },         // JSON.ARRLEN key [path]
    JsonArrPop { key: String, path: String, index: Option<isize> }, // JSON.ARRPOP key path [index]
    JsonArrTrim { key: String, path: String, start: isize, stop: isize }, // JSON.ARRTRIM key path start stop

    // Graph Commands
    GraphCreateNode { key: String, node_id: String, properties: Vec<(String, String)> }, // GRAPH.CREATE_NODE key node_id [property key value ...]
    GraphGetNode { key: String, node_id: String },    // GRAPH.GET_NODE key node_id
    GraphDeleteNode { key: String, node_id: String }, // GRAPH.DELETE_NODE key node_id

    GraphCreateEdge { key: String, edge_id: String, source_node_id: String, target_node_id: String, relation_type: String, properties: Vec<(String, String)> },
    GraphGetEdge { key: String, edge_id: String },
    GraphDeleteEdge { key: String, edge_id: String },

    GraphGetNodeProperties { key: String, node_id: String }, // GRAPH.GET_NODE_PROPERTIES key node_id
    GraphSetNodeProperty { key: String, node_id: String, property_key: String, property_value: String }, // GRAPH.SET_NODE_PROPERTY key node_id property_key property_value
    GraphDeleteNodeProperty { key: String, node_id: String, property_key: String }, // GRAPH.DELETE_NODE_PROPERTY key node_id property_key

    GraphGetEdgeProperties { key: String, edge_id: String }, // GRAPH.GET_EDGE_PROPERTIES key edge_id
    GraphSetEdgeProperty { key: String, edge_id: String, property_key: String, property_value: String }, // GRAPH.SET_EDGE_PROPERTY key edge_id property_key
    GraphDeleteEdgeProperty { key: String, edge_id: String, property_key: String }, // GRAPH.DELETE_EDGE_PROPERTY key edge_id property_key

    Unknown(String),
}

impl TryFrom<&[Value]> for Command {
    type Error = &'static str;

    fn try_from(values: &[Value]) -> Result<Self, Self::Error> {
        if values.is_empty() {
            return Err("Empty Command");
        }

        match &values[0] {
            Value::BulkString(bs) => {
                match std::str::from_utf8(bs) {
                    Ok(command_str) => {
                        let command_upper = command_str.to_uppercase();
                        match command_upper.as_str() {
                            // connection commands
                            "PING" => {
                                if values.len() == 1 {
                                    Ok(Command::Ping {  })
                                } else {
                                    Err("PING command expectes no arguments")
                                }
                            }
                            "ECHO" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(msg_bs) = &values[1] {
                                        let message = String::from_utf8_lossy(msg_bs).into_owned();
                                        Ok(Command::Echo { message })
                                    } else {
                                        Err("ECHO command argument must be a bulk string")
                                    }
                                } else {
                                    Err("ECHO command expects exactly one argument")
                                }
                            },
                            "COMMAND" => {
                                if values.len() == 1 {
                                    Ok(Command::Command {})
                                } else if values.len() == 2 {
                                    if let Value::BulkString(sub_command_bs) = &values[1] {
                                        let sub_command = String::from_utf8_lossy(sub_command_bs).to_uppercase();
                                        match sub_command.as_str() {
                                            "LIST" => Ok(Command::CommandList {}),
                                            "HELP" => Err("COMMAND HELP expects a command name argument"), // COMMAND HELP without command name is invalid
                                            "INFO" => Err("COMMAND INFO expects a command name argument"), // COMMAND INFO without command name is invalid
                                            _ => Ok(Command::Unknown(command_str.to_string())), // Treat as unknown if subcommand not recognized
                                        }
                                    } else {
                                        Err("COMMAND subcommand must be a bulk string")
                                    }
                                } else if values.len() == 3 {
                                    if let Value::BulkString(sub_command_bs) = &values[1] {
                                        let sub_command = String::from_utf8_lossy(sub_command_bs).to_uppercase();
                                        if let Value::BulkString(command_name_bs) = &values[2] {
                                            let command_name = String::from_utf8_lossy(command_name_bs).into_owned();
                                            match sub_command.as_str() {
                                                "INFO" => Ok(Command::CommandInfo { command_name }),
                                                "HELP" => Ok(Command::CommandHelp { command_name }),
                                                _ => Ok(Command::Unknown(command_str.to_string())), // Treat as unknown if subcommand not recognized
                                            }
                                        } else {
                                            Err("COMMAND INFO/HELP command name argument must be a bulk string")
                                        }
                                    } else {
                                        Err("COMMAND subcommand must be a bulk string")
                                    }
                                }
                                else {
                                    Ok(Command::Command {}) // Default COMMAND with no or invalid args
                                }
                            }

                            "CLIENT" => {
                                if values.len() == 1 {
                                    Ok(Command::Client { subcommand: ClientSubCommand::NoSubCommand }) // Bare CLIENT command
                                } else if values.len() >= 2 {
                                    if let Value::BulkString(sub_command_bs) = &values[1] {
                                        let sub_command_str = String::from_utf8_lossy(sub_command_bs).to_uppercase();
                                        match sub_command_str.as_str() {
                                            "LIST" => Ok(Command::Client { subcommand: ClientSubCommand::List }),
                                            "INFO" => Ok(Command::Client { subcommand: ClientSubCommand::Info }),
                                            "GETNAME" => Ok(Command::Client { subcommand: ClientSubCommand::GetName }),
                                            "SETNAME" => {
                                                if values.len() == 3 {
                                                    if let Value::BulkString(connection_name_bs) = &values[2] {
                                                        let connection_name = String::from_utf8_lossy(connection_name_bs).into_owned();
                                                        Ok(Command::Client { subcommand: ClientSubCommand::SetName { connection_name } })
                                                    } else {
                                                        Err("CLIENT SETNAME connection name must be a bulk string")
                                                    }
                                                } else {
                                                    Err("CLIENT SETNAME expects exactly one argument (connection name)")
                                                }
                                            },
                                            "KILL" => {
                                                if values.len() == 3 {
                                                    if let Value::BulkString(ip_port_bs) = &values[2] {
                                                        let ip_port = String::from_utf8_lossy(ip_port_bs).into_owned();
                                                        Ok(Command::Client { subcommand: ClientSubCommand::Kill { ip_port } })
                                                    } else {
                                                        Err("CLIENT KILL ip:port must be a bulk string")
                                                    }

                                                }  else {
                                                    Err("CLIENT KILL expects exactly one argument (ip:port)")
                                                }
                                            },

                                            _ => Ok(Command::Client { subcommand: ClientSubCommand::NoSubCommand }), // Treat as CLIENT with no subcommand if not recognized
                                        }
                                    } else {
                                        Err("CLIENT subcommand must be a bulk string")
                                    }
                                } else {
                                    Ok(Command::Client { subcommand: ClientSubCommand::NoSubCommand }) // Default CLIENT
                                }
                            },

                            // Generic Commands
                            "TYPE" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Type { key })
                                    } else {
                                        Err("TYPE command argument must be a bulk string")
                                    }
                                } else {
                                    Err("TYPE command expects exactly one argument (key)")
                                }
                            }

                            "EXISTS" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Exists { key })
                                    } else {
                                        Err("EXISTS command argument must be a bulk string")
                                    }
                                } else {
                                    Err("EXISTS command expects exactly one argument (key)")
                                }
                            }
                            "DEL" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Del { key })
                                    } else {
                                        Err("DEL command argument must be a bulk string")
                                    }
                                } else {
                                    Err("DEL command expects exactly one argument (key)")
                                }
                            }
                            "KEYS" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(pattern_bs) = &values[1] {
                                        let pattern = String::from_utf8_lossy(pattern_bs).into_owned();
                                        Ok(Command::Keys { pattern })
                                    } else {
                                        Err("KEYS command argument must be a bulk string")
                                    }
                                } else {
                                    Err("KEYS command expects exactly one argument (pattern)")
                                }
                            }

                            "EXPIRE" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(seconds_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            if let Ok(seconds_str) = std::str::from_utf8(&seconds_bs) {
                                                if let Ok(seconds) = seconds_str.parse::<u64>() {
                                                    Ok(Command::Expire{ key, seconds})
                                                } else {
                                                    Err("EXPIRE seconds argument must be a valid unsigned integer")
                                                }
                                            } else {
                                                Err("EXPIRE seconds argument must be a valid unsigned integer")
                                            }
                                        } else {
                                            Err("EXPIRE seconds argument must be a bulk string")
                                        }
                                    } else {
                                        Err("EXPIRE key argument must be a bulk string")
                                    }
                                } else {
                                    Err("EXPIRE command expects exactly two arguments (key and seconds)")
                                }
                            }
                            "TTL" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Ttl { key })
                                    } else {
                                        Err("TTL command argument must be a bulk string")
                                    }
                                } else {
                                    Err("TTL command expects exactly one argument (key)")
                                }
                            }
                            "PERSIST" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Persist { key })
                                    } else {
                                        Err("PERSIST command argument must be a bulk string")
                                    }
                                } else {
                                    Err("PERSIST command expects exactly one argument (key)")
                                }
                            }
                            // String commands
                            "SET" => {
                                let mut key: Option<String> = None;
                                let mut value: Option<String> = None;
                                let mut expire_option: Option<SetExpireOption> = None;
                                let mut set_option: Option<SetOption> = None;

                                let mut i = 1;
                                while i < values.len() {
                                    match i {
                                        1 => {
                                            if let Value::BulkString(key_bs) = &values[i] {
                                                key = Some(String::from_utf8_lossy(key_bs).into_owned());
                                            } else {
                                                return Err("SET key must be a bulk string");
                                            }
                                        },
                                        2 => {
                                            if let Value::BulkString(value_bs) = &values[i] {
                                                value = Some(String::from_utf8_lossy(value_bs).into_owned());
                                            } else {
                                                return Err("SET value must be a bulk string");
                                            }
                                        },
                                        _ => {
                                            if let Value::BulkString(option_bs) = &values[i] {
                                                let option_str = String::from_utf8_lossy(option_bs).to_uppercase();
                                                match option_str.as_str() {
                                                    "EX" | "PX" => {
                                                        if i + 1 < values.len() {
                                                            if let Value::BulkString(time_bs) = &values[i+1] {
                                                                if let Ok(time_val) = String::from_utf8_lossy(time_bs).parse::<u64>() {
                                                                    match option_str.as_str() {
                                                                        "EX" => expire_option = Some(SetExpireOption::EX(time_val)),
                                                                        "PX" => expire_option = Some(SetExpireOption::PX(time_val)),
                                                                        _ => unreachable!() // Should not happen
                                                                    }
                                                                    i += 1; // consume next argument (time value)
                                                                } else {
                                                                    return Err("SET EX/PX value is not a valid number");
                                                                }
                                                            } else {
                                                                return Err("SET EX/PX value must be a bulk string");
                                                            }
                                                        } else {
                                                            return Err("SET EX/PX option requires a time argument");
                                                        }
                                                    },
                                                    "NX" => set_option = Some(SetOption::NX),
                                                    "XX" => set_option = Some(SetOption::XX),
                                                    _ => return Err("SET invalid option, use EX, PX, NX, XX"),
                                                }
                                            } else {
                                                return Err("SET option must be a bulk string");
                                            }
                                        }
                                    }
                                    i += 1;
                                }

                                if let Some(key_val) = key {
                                    if let Some(value_val) = value {
                                        Ok(Command::Set { key: key_val, value: value_val, expire_option, set_option })
                                    } else {
                                        Err("SET command requires a value argument")
                                    }
                                } else {
                                    Err("SET command requires a key argument")
                                }
                            }
                            "GET" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Get { key })
                                    } else {
                                        Err("GET command argument must be a bulk string")
                                    }
                                } else {
                                    Err("GET command expects exactly one argument (key)")
                                }
                            }
                            "APPEND" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(value_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let value = String::from_utf8_lossy(value_bs).into_owned();
                                            Ok(Command::Append { key, value })
                                        } else {
                                            Err("APPEND value must be a bulk string")
                                        }
                                    } else {
                                        Err("APPEND key must be a bulk string")
                                    }
                                } else {
                                    Err("APPEND command expects exactly two arguments (key and value)")
                                }
                            }
                            "STRLEN" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::StrLen { key })
                                    } else {
                                        Err("STRLEN command argument must be a bulk string")
                                    }
                                } else {
                                    Err("STRLEN command expects exactly one argument (key)")
                                }
                            }
                            "GETRANGE" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(start_bs) = &values[2] {
                                            if let Value::BulkString(end_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let start = String::from_utf8_lossy(start_bs).into_owned().parse::<isize>().map_err(|_| "GETRANGE start is not an integer")?;
                                                let end = String::from_utf8_lossy(end_bs).into_owned().parse::<isize>().map_err(|_| "GETRANGE end is not an integer")?;
                                                Ok(Command::GetRange { key, start, end })
                                            } else {
                                                Err("GETRANGE end must be a bulk string")
                                            }
                                        } else {
                                            Err("GETRANGE start must be a bulk string")
                                        }
                                    } else {
                                        Err("GETRANGE key must be a bulk string")
                                    }
                                } else {
                                    Err("GETRANGE command expects exactly three arguments (key, start, end)")
                                }
                            }
                            "SETRANGE" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(offset_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let offset = String::from_utf8_lossy(offset_bs).into_owned().parse::<usize>().map_err(|_| "SETRANGE offset is not an unsigned integer")?;
                                                let value = String::from_utf8_lossy(value_bs).into_owned();
                                                Ok(Command::SetRange { key, offset, value })
                                            } else {
                                                Err("SETRANGE value must be a bulk string")
                                            }
                                        } else {
                                            Err("SETRANGE offset must be a bulk string")
                                        }
                                    } else {
                                        Err("SETRANGE key must be a bulk string")
                                    }
                                } else {
                                    Err("SETRANGE command expects exactly three arguments (key, offset, value)")
                                }
                            }
                            "GETSET" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(value_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let value = String::from_utf8_lossy(value_bs).into_owned();
                                            Ok(Command::GetSet { key, value })
                                        } else {
                                            Err("GETSET value must be a bulk string")
                                        }
                                    } else {
                                        Err("GETSET key must be a bulk string")
                                    }
                                } else {
                                    Err("GETSET command expects exactly two arguments (key and value)")
                                }
                            }
                            "MGET" => {
                                if values.len() >= 2 {
                                    let keys: Vec<String> = values[1..].iter().map(|val| {
                                        if let Value::BulkString(key_bs) = val {
                                            String::from_utf8_lossy(key_bs).into_owned()
                                        } else {
                                            String::new() // Placeholder, error will be handled later if empty key
                                        }
                                    }).collect();
                                    Ok(Command::MGet { keys })
                                } else {
                                    Err("MGET command expects at least one key argument")
                                }
                            }
                            "MSET" => {
                                if values.len() >= 3 && values.len() % 2 == 1 {
                                    let mut key_values: Vec<(String, String)> = Vec::new();
                                    let mut i = 1;
                                    while i < values.len() {
                                        if let (Value::BulkString(key_bs), Value::BulkString(value_bs)) = (&values[i], &values[i+1]) {
                                            key_values.push((String::from_utf8_lossy(key_bs).into_owned(), String::from_utf8_lossy(value_bs).into_owned()));
                                            i += 2;
                                        } else {
                                            return Err("MSET command expects key value pairs as bulk strings");
                                        }
                                    }
                                    Ok(Command::MSet { key_values })
                                } else {
                                    Err("MSET command expects key value pairs, odd number of arguments including command name")
                                }
                            }
                            
                            "INCR" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Incr { key })
                                    } else {
                                        Err("INCR command argument must be a bulk string")
                                    }
                                } else {
                                    Err("INCR command expects exactly one argument (key)")
                                }
                            }
                            "DECR" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Decr { key })
                                    } else {
                                        Err("DECR command argument must be a bulk string")
                                    }
                                } else {
                                    Err("DECR command expects exactly one argument (key)")
                                }
                            }

                            "INCRBY" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(increment_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let increment = String::from_utf8_lossy(increment_bs).into_owned().parse::<i64>().map_err(|_| "INCRBY increment is not an integer")?;
                                            Ok(Command::IncrBy { key, increment })
                                        } else {
                                            Err("INCRBY increment must be a bulk string")
                                        }
                                    } else {
                                        Err("INCRBY key must be a bulk string")
                                    }
                                } else {
                                    Err("INCRBY command expects exactly two arguments (key and increment)")
                                }
                            }
                            "DECRBY" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(decrement_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let decrement = String::from_utf8_lossy(decrement_bs).into_owned().parse::<i64>().map_err(|_| "DECRBY decrement is not an integer")?;
                                            Ok(Command::DecrBy { key, decrement })
                                        } else {
                                            Err("DECRBY decrement must be a bulk string")
                                        }
                                    } else {
                                        Err("DECRBY key must be a bulk string")
                                    }
                                } else {
                                    Err("DECRBY command expects exactly two arguments (key and decrement)")
                                }
                            }

                            "INCRBYFLOAT" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(increment_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let increment = String::from_utf8_lossy(increment_bs).into_owned().parse::<f64>().map_err(|_| "INCRBYFLOAT increment is not a float")?;
                                            Ok(Command::IncrByFloat { key, increment })
                                        } else {
                                            Err("INCRBYFLOAT increment must be a bulk string")
                                        }
                                    } else {
                                        Err("INCRBYFLOAT key must be a bulk string")
                                    }
                                } else {
                                    Err("INCRBYFLOAT command expects exactly two arguments (key and increment)")
                                }
                            }

                            // Hash Commands
                            "HSET" => {
                                if values.len() >= 3 && (values.len() - 1) % 2 == 0 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut field_values: Vec<(String, String)> = Vec::new();
                                        let mut i = 2;
                                        while i < values.len() {
                                            if let (Value::BulkString(field_bs), Value::BulkString(value_bs)) = (&values[i], &values[i+1]) {
                                                field_values.push((String::from_utf8_lossy(field_bs).into_owned(), String::from_utf8_lossy(value_bs).into_owned()));
                                                i += 2;
                                            } else {
                                                return Err("HSET command expects field value pairs as bulk strings after key");
                                            }
                                        }
                                        Ok(Command::HSet { key, field_values })
                                    } else {
                                        Err("HSET command key must be a bulk string")
                                    }
                                } else {
                                    Err("HSET command expects at least one field-value pair after key, even number of arguments after command name and key")
                                }
                            }

                            "HGET" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(field_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let field = String::from_utf8_lossy(field_bs).into_owned();
                                            Ok(Command::HGet { key, field })
                                        } else {
                                            Err("HGET command field must be a bulk string")
                                        }
                                    } else {
                                        Err("HGET command key must be a bulk string")
                                    }
                                } else {
                                    Err("HGET command expects exactly two arguments (key, field)")
                                }
                            }

                            "HDEL" => {
                                if values.len() >= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let fields: Vec<String> = values[2..].iter().map(|val| {
                                            if let Value::BulkString(field_bs) = val {
                                                String::from_utf8_lossy(field_bs).into_owned()
                                            } else {
                                                String::new() // Placeholder, error will be handled later if empty field
                                            }
                                        }).collect();
                                        Ok(Command::HDel { key, fields })
                                    } else {
                                        Err("HDEL command key must be a bulk string")
                                    }
                                } else {
                                    Err("HDEL command expects at least one field argument after key")
                                }
                            }
                            
                            "HEXISTS" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(field_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let field = String::from_utf8_lossy(field_bs).into_owned();
                                            Ok(Command::HExists { key, field })
                                        } else {
                                            Err("HEXISTS command field must be a bulk string")
                                        }
                                    } else {
                                        Err("HEXISTS command key must be a bulk string")
                                    }
                                } else {
                                    Err("HEXISTS command expects exactly two arguments (key and field)")
                                }
                            }

                            "HGETALL" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::HGetAll { key })
                                    } else {
                                        Err("HGETALL command key must be a bulk string")
                                    }
                                } else {
                                    Err("HGETALL command expects exactly one argument (key)")
                                }
                            }

                            "HINCRBY" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(field_bs) = &values[2] {
                                            if let Value::BulkString(increment_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let field = String::from_utf8_lossy(field_bs).into_owned();
                                                let increment = String::from_utf8_lossy(increment_bs).into_owned().parse::<i64>().map_err(|_| "HINCRBY increment is not an integer")?;
                                                Ok(Command::HIncrBy { key, field, increment })
                                            } else {
                                                Err("HINCRBY increment must be a bulk string")
                                            }
                                        } else {
                                            Err("HINCRBY field must be a bulk string")
                                        }
                                    } else {
                                        Err("HINCRBY key must be a bulk string")
                                    }
                                } else {
                                    Err("HINCRBY command expects exactly three arguments (key, field, increment)")
                                }
                            }

                            "HINCRBYFLOAT" => {
                                if values.len() >= 4 && (values.len() - 2) % 2 == 0 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut key_field_increment: Vec<(String, f64)> = Vec::new();
                                        let mut i = 2;
                                        while i < values.len() {
                                            if let (Value::BulkString(field_bs), Value::BulkString(increment_bs)) = (&values[i], &values[i+1]) {
                                                let field = String::from_utf8_lossy(field_bs).into_owned();
                                                let increment = String::from_utf8_lossy(increment_bs).into_owned().parse::<f64>().map_err(|_| "HINCRBYFLOAT increment is not a float")?;
                                                key_field_increment.push((field, increment));
                                                i += 2;
                                            } else {
                                                return Err("HINCRBYFLOAT command expects field increment pairs as bulk strings after key");
                                            }
                                        }
                                        Ok(Command::HIncrByFloat { key, key_field_increment })
                                    } else {
                                        Err("HINCRBYFLOAT command key must be a bulk string")
                                    }
                                } else {
                                    Err("HINCRBYFLOAT command expects at least one field-increment pair after key, even number of arguments after command name and key")
                                }
                            }

                            "HKEYS" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::HKeys { key })
                                    } else {
                                        Err("HKEYS command key must be a bulk string")
                                    }
                                } else {
                                    Err("HKEYS command expects exactly one argument (key)")
                                }
                            }

                            "HLEN" => todo!(),

                            "HMGET" => {
                                if values.len() >= 2 { // HMGET can have one or more fields after key
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut fields: Vec<String> = Vec::new();
                                        for i in 2..values.len() {
                                            if let Value::BulkString(field_bs) = &values[i] {
                                                fields.push(String::from_utf8_lossy(field_bs).into_owned());
                                            } else {
                                                return Err("HMGET fields must be bulk strings");
                                            }
                                        }
                                        Ok(Command::HMGet { key, fields })
                                    } else {
                                        Err("HMGET command key must be a bulk string")
                                    }
                                } else {
                                    Err("HMGET command expects at least one argument (key)")
                                }
                            }

                            "HMSET" => { // ... (HMSET parsing similar to HSET, taking key and field_values) ...
                                if values.len() >= 3 && (values.len() - 1) % 2 == 0 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut field_values: Vec<(String, String)> = Vec::new();
                                        let mut i = 2;
                                        while i < values.len() {
                                            if let (Value::BulkString(field_bs), Value::BulkString(value_bs)) = (&values[i], &values[i+1]) {
                                                field_values.push((String::from_utf8_lossy(field_bs).into_owned(), String::from_utf8_lossy(value_bs).into_owned()));
                                                i += 2;
                                            } else {
                                                return Err("HMSET command expects field value pairs as bulk strings after key");
                                            }
                                        }
                                        Ok(Command::HMSet { key, field_values })
                                    } else {
                                        Err("HMSET command key must be a bulk string")
                                    }
                                } else {
                                    Err("HMSET command expects at least one field-value pair after key, even number of arguments after command name and key")
                                }
                            }

                            "HSETNX" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(field_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let field = String::from_utf8_lossy(field_bs).into_owned();
                                                let value = String::from_utf8_lossy(value_bs).into_owned();
                                                Ok(Command::HSetNx { key, field, value })
                                            } else {
                                                Err("HSETNX value must be a bulk string")
                                            }
                                        } else {
                                            Err("HSETNX field must be a bulk string")
                                        }
                                    } else {
                                        Err("HSETNX key must be a bulk string")
                                    }
                                } else {
                                    Err("HSETNX command expects exactly three arguments (key, field, value)")
                                }
                            }

                            "HVALS" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::HVals { key })
                                    } else {
                                        Err("HVALS command key must be a bulk string")
                                    }
                                } else {
                                    Err("HVALS command expects exactly one argument (key)")
                                }
                            }
                            
                            // List commands
                            "LPUSH" => {
                                if values.len() >= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let values_list: Vec<String> = values[2..].iter().map(|val| {
                                            if let Value::BulkString(value_bs) = val {
                                                String::from_utf8_lossy(value_bs).into_owned()
                                            } else {
                                                String::new() // Placeholder, error handled later if empty value
                                            }
                                        }).collect();
                                        Ok(Command::ListLPush { key, values: values_list })
                                    } else {
                                        Err("LPUSH command key must be a bulk string")
                                    }
                                } else {
                                    Err("LPUSH command expects at least one value argument after key")
                                }
                            }

                            "RPUSH" => {
                                if values.len() >= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let values_list: Vec<String> = values[2..].iter().map(|val| {
                                            if let Value::BulkString(value_bs) = val {
                                                String::from_utf8_lossy(value_bs).into_owned()
                                            } else {
                                                String::new() // Placeholder, error handled later if empty value
                                            }
                                        }).collect();
                                        Ok(Command::ListRPush { key, values: values_list })
                                    } else {
                                        Err("RPUSH command key must be a bulk string")
                                    }
                                } else {
                                    Err("RPUSH command expects at least one value argument after key")
                                }
                            }

                            "LPOP" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::LPop { key })
                                    } else {
                                        Err("LPOP command key must be a bulk string")
                                    }
                                } else {
                                    Err("LPOP command expects exactly one argument (key)")
                                }
                            }

                            "RPOP" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::RPop { key })
                                    } else {
                                        Err("RPOP command key must be a bulk string")
                                    }
                                } else {
                                    Err("RPOP command expects exactly one argument (key)")
                                }
                            }

                            "LLEN" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::LLen { key })
                                    } else {
                                        Err("LLEN command key must be a bulk string")
                                    }
                                } else {
                                    Err("LLEN command expects exactly one argument (key)")
                                }
                            }

                            "LRANGE" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(start_bs) = &values[2] {
                                            if let Value::BulkString(end_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let start = String::from_utf8_lossy(start_bs).into_owned().parse::<isize>().map_err(|_| "LRANGE start is not an integer")?;
                                                let end = String::from_utf8_lossy(end_bs).into_owned().parse::<isize>().map_err(|_| "LRANGE end is not an integer")?;
                                                Ok(Command::LRange { key, start, end })
                                            } else {
                                                Err("LRANGE end must be a bulk string")
                                            }
                                        } else {
                                            Err("LRANGE start must be a bulk string")
                                        }
                                    } else {
                                        Err("LRANGE key must be a bulk string")
                                    }
                                } else {
                                    Err("LRANGE command expects exactly three arguments (key, start, end)")
                                }
                            }

                            "LINDEX" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(index_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let index = String::from_utf8_lossy(index_bs).into_owned().parse::<isize>().map_err(|_| "LINDEX index is not an integer")?;
                                            Ok(Command::LIndex { key, index })
                                        } else {
                                            Err("LINDEX index must be a bulk string")
                                        }
                                    } else {
                                        Err("LINDEX key must be a bulk string")
                                    }
                                } else {
                                    Err("LINDEX command expects exactly two arguments (key and index)")
                                }
                            }
                            
                            "LINSERT" => {
                                if values.len() == 5 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(before_after_bs) = &values[2] {
                                            let before_after_str = String::from_utf8_lossy(before_after_bs).to_uppercase();
                                            let before_after = match before_after_str.as_str() {
                                                "BEFORE" => InsertOption::Before,
                                                "AFTER" => InsertOption::After,
                                                _ => return Err("LINSERT option must be BEFORE or AFTER"),
                                            };
                                            if let Value::BulkString(pivot_bs) = &values[3] {
                                                if let Value::BulkString(value_bs) = &values[4] {
                                                    let key = String::from_utf8_lossy(key_bs).into_owned();
                                                    let pivot = String::from_utf8_lossy(pivot_bs).into_owned();
                                                    let value = String::from_utf8_lossy(value_bs).into_owned();
                                                    Ok(Command::LInsert { key, before_after, pivot, value })
                                                } else {
                                                    Err("LINSERT value must be a bulk string")
                                                }
                                            } else {
                                                Err("LINSERT pivot must be a bulk string")
                                            }
                                        } else {
                                            Err("LINSERT BEFORE|AFTER must be a bulk string")
                                        }
                                    } else {
                                        Err("LINSERT key must be a bulk string")
                                    }
                                } else {
                                    Err("LINSERT command expects exactly four arguments (key, BEFORE|AFTER, pivot, value)")
                                }
                            }

                            "LSET" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(index_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let index = String::from_utf8_lossy(index_bs).into_owned().parse::<isize>().map_err(|_| "LSET index is not an integer")?;
                                                let value = String::from_utf8_lossy(value_bs).into_owned();
                                                Ok(Command::LSet { key, index, value })
                                            } else {
                                                Err("LSET value must be a bulk string")
                                            }
                                        } else {
                                            Err("LSET index must be a bulk string")
                                        }
                                    } else {
                                        Err("LSET key must be a bulk string")
                                    }
                                } else {
                                    Err("LSET command expects exactly three arguments (key, index, value)")
                                }
                            }

                            "LTRIM" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(start_bs) = &values[2] {
                                            if let Value::BulkString(end_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let start = String::from_utf8_lossy(start_bs).into_owned().parse::<isize>().map_err(|_| "LTRIM start is not an integer")?;
                                                let end = String::from_utf8_lossy(end_bs).into_owned().parse::<isize>().map_err(|_| "LTRIM end is not an integer")?;
                                                Ok(Command::LTrim { key, start, end })
                                            } else {
                                                Err("LTRIM end must be a bulk string")
                                            }
                                        } else {
                                            Err("LTRIM start must be a bulk string")
                                        }
                                    } else {
                                        Err("LTRIM key must be a bulk string")
                                    }
                                } else {
                                    Err("LTRIM command expects exactly three arguments (key, start, end)")
                                }
                            }

                            "LREM" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(count_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let count = String::from_utf8_lossy(count_bs).into_owned().parse::<i64>().map_err(|_| "LREM count is not an integer")?;
                                                let value = String::from_utf8_lossy(value_bs).into_owned();
                                                Ok(Command::LRem { key, count, value })
                                            } else {
                                                Err("LREM value must be a bulk string")
                                            }
                                        } else {
                                            Err("LREM count must be a bulk string")
                                        }
                                    } else {
                                        Err("LREM key must be a bulk string")
                                    }
                                } else {
                                    Err("LREM command expects exactly three arguments (key, count, value)")
                                }
                            }

                            "RPOPLPUSH" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(source_bs) = &values[1] {
                                        if let Value::BulkString(destination_bs) = &values[2] {
                                            let source = String::from_utf8_lossy(source_bs).into_owned();
                                            let destination = String::from_utf8_lossy(destination_bs).into_owned();
                                            Ok(Command::RPopLPush { source, destination })
                                        } else {
                                            Err("RPOPLPUSH destination must be a bulk string")
                                        }
                                    } else {
                                        Err("RPOPLPUSH source must be a bulk string")
                                    }
                                } else {
                                    Err("RPOPLPUSH command expects exactly two arguments (source and destination)")
                                }
                            }

                            "BLPOP" => {
                                if values.len() >= 3 {
                                    let mut keys: Vec<String> = Vec::new();
                                    for i in 1..values.len()-1 {
                                        if let Value::BulkString(key_bs) = &values[i] {
                                            keys.push(String::from_utf8_lossy(key_bs).into_owned());
                                        } else {
                                            return Err("BLPOP keys must be bulk strings");
                                        }
                                    }
                                    if let Value::BulkString(timeout_bs) = &values.last().unwrap() {
                                        let timeout = String::from_utf8_lossy(timeout_bs).into_owned().parse::<f64>().map_err(|_| "BLPOP timeout is not a float")?;
                                        Ok(Command::BLPop { keys, timeout })
                                    } else {
                                        Err("BLPOP timeout must be a bulk string")
                                    }


                                } else {
                                    Err("BLPOP command expects at least one key and a timeout")
                                }
                            }

                            "BRPOP" => {
                                if values.len() >= 3 {
                                    let mut keys: Vec<String> = Vec::new();
                                    for i in 1..values.len()-1 {
                                        if let Value::BulkString(key_bs) = &values[i] {
                                            keys.push(String::from_utf8_lossy(key_bs).into_owned());
                                        } else {
                                            return Err("BRPOP keys must be bulk strings");
                                        }
                                    }
                                    if let Value::BulkString(timeout_bs) = &values.last().unwrap() {
                                        let timeout = String::from_utf8_lossy(timeout_bs).into_owned().parse::<f64>().map_err(|_| "BRPOP timeout is not a float")?;
                                        Ok(Command::BRPop { keys, timeout })
                                    } else {
                                        Err("BRPOP timeout must be a bulk string")
                                    }
                                } else {
                                    Err("BRPOP command expects at least one key and a timeout")
                                }
                            }

                            "JSON.SET" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let path = String::from_utf8_lossy(path_bs).into_owned();
                                                let value = String::from_utf8_lossy(value_bs).into_owned();
                                                Ok(Command::JsonSet { key, path, value })
                                            } else {
                                                Err("JSON.SET value must be a bulk string")
                                            }
                                        } else {
                                            Err("JSON.SET path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.SET key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.SET command expects exactly three arguments (key, path, value)")
                                }
                            }

                            "JSON.GET" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                String::from_utf8_lossy(path_bs).into_owned()
                                            } else {
                                                return Err("JSON.GET path must be a bulk string");
                                            }
                                        } else {
                                            ".".to_string() // Default path to root if not provided
                                        };
                                        Ok(Command::JsonGet { key, path })
                                    } else {
                                        Err("JSON.GET key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.GET command expects one or two arguments (key, [path])")
                                }
                            }
                            
                            "JSON.DEL" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                String::from_utf8_lossy(path_bs).into_owned()
                                            } else {
                                                return Err("JSON.DEL path must be a bulk string");
                                            }
                                        } else {
                                            ".".to_string() // Default path to root if not provided
                                        };
                                        Ok(Command::JsonDel { key, path })
                                    } else {
                                        Err("JSON.DEL key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.DEL command expects one or two arguments (key, [path])")
                                }
                            }

                            "JSON.TYPE" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                String::from_utf8_lossy(path_bs).into_owned()
                                            } else {
                                                return Err("JSON.TYPE path must be a bulk string");
                                            }
                                        } else {
                                            ".".to_string() // Default path to root if not provided
                                        };
                                        Ok(Command::JsonType { key, path })
                                    } else {
                                        Err("JSON.TYPE key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.TYPE command expects one or two arguments (key, [path])")
                                }
                            }

                            "JSON.NUMINCRBY" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(increment_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let path = String::from_utf8_lossy(path_bs).into_owned();
                                                let increment = String::from_utf8_lossy(increment_bs).into_owned().parse::<f64>().map_err(|_| "JSON.NUMINCRBY increment is not a float")?;
                                                Ok(Command::JsonNumIncrBy { key, path, increment })
                                            } else {
                                                Err("JSON.NUMINCRBY increment must be a bulk string")
                                            }
                                        } else {
                                            Err("JSON.NUMINCRBY path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.NUMINCRBY key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.NUMINCRBY command expects exactly three arguments (key, path, increment)")
                                }
                            }

                            "JSON.STRAPPEND" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let path = String::from_utf8_lossy(path_bs).into_owned();
                                                let value = String::from_utf8_lossy(value_bs).into_owned();
                                                Ok(Command::JsonStrAppend { key, path, value })
                                            } else {
                                                Err("JSON.STRAPPEND value must be a bulk string")
                                            }
                                        } else {
                                            Err("JSON.STRAPPEND path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.STRAPPEND key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.STRAPPEND command expects exactly three arguments (key, path, value)")
                                }
                            }

                            "JSON.ARRAPPEND" => {
                                if values.len() >= 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let path = String::from_utf8_lossy(path_bs).into_owned();
                                            let mut values_list: Vec<String> = Vec::new();
                                            for i in 3..values.len() {
                                                if let Value::BulkString(value_bs) = &values[i] {
                                                    values_list.push(String::from_utf8_lossy(value_bs).into_owned());
                                                } else {
                                                    return Err("JSON.ARRAPPEND values must be bulk strings");
                                                }
                                            }
                                            Ok(Command::JsonArrAppend { key, path, values: values_list })
                                        } else {
                                            Err("JSON.ARRAPPEND path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.ARRAPPEND key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.ARRAPPEND command expects at least two value arguments after key and path")
                                }
                            }

                            "JSON.OBJSET" => {
                                if values.len() == 5 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(key_to_set_bs) = &values[3] {
                                                if let Value::BulkString(value_bs) = &values[4] {
                                                    let key = String::from_utf8_lossy(key_bs).into_owned();
                                                    let path = String::from_utf8_lossy(path_bs).into_owned();
                                                    let key_to_set = String::from_utf8_lossy(key_to_set_bs).into_owned();
                                                    let value = String::from_utf8_lossy(value_bs).into_owned();
                                                    Ok(Command::JsonObjSet { key, path, key_to_set, value })
                                                } else {
                                                    return Err("JSON.OBJSET value must be a bulk string")
                                                }
                                            } else {
                                                return Err("JSON.OBJSET object key to set must be a bulk string")
                                            }
                                        } else {
                                            Err("JSON.OBJSET path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.OBJSET key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.OBJSET command expects exactly four arguments (key, path, object-key, value)")
                                }
                            }

                            "JSON.OBJKEYS" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                String::from_utf8_lossy(path_bs).into_owned()
                                            } else {
                                                return Err("JSON.OBJKEYS path must be a bulk string");
                                            }
                                        } else {
                                            ".".to_string() // Default path to root if not provided
                                        };
                                        Ok(Command::JsonObjKeys { key, path })
                                    } else {
                                        Err("JSON.OBJKEYS key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.OBJKEYS command expects one or two arguments (key, [path])")
                                }
                            }

                            "JSON.OBJLEN" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                String::from_utf8_lossy(path_bs).into_owned()
                                            } else {
                                                return Err("JSON.OBJLEN path must be a bulk string");
                                            }
                                        } else {
                                            ".".to_string() // Default path to root if not provided
                                        };
                                        Ok(Command::JsonObjLen { key, path })
                                    } else {
                                        Err("JSON.OBJLEN key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.OBJLEN command expects one or two arguments (key, [path])")
                                }
                            }

                            "JSON.ARRINDEX" => {
                                if values.len() >= 4 && values.len() <= 5 { // JSON.ARRINDEX key path value [start [stop]]
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let path = String::from_utf8_lossy(path_bs).into_owned();
                                                let value = String::from_utf8_lossy(value_bs).into_owned();
                                                let range = if values.len() == 5 {
                                                    if let Value::BulkString(range_start_bs) = &values[4] {
                                                        let start = String::from_utf8_lossy(range_start_bs).into_owned().parse::<isize>().map_err(|_| "JSON.ARRINDEX start is not an integer")?;
                                                        Some((start, -1)) // Stop is optional in our command enum, using -1 as placeholder, will adjust in cmd handler if needed
                                                    } else {
                                                        return Err("JSON.ARRINDEX start must be a bulk string");
                                                    }
                                                } else {
                                                    None
                                                };
                                                Ok(Command::JsonArrIndex { key, path, value, range })
                                            } else {
                                                return Err("JSON.ARRINDEX value to find must be a bulk string");
                                            }
                                        } else {
                                            Err("JSON.ARRINDEX path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.ARRINDEX key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.ARRINDEX command expects three to four arguments (key, path, value, [start])")
                                }
                            }

                            "JSON.ARRINSERT" => {
                                if values.len() >= 5 { // JSON.ARRINSERT key path index value [value ...]
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(index_bs) = &values[3] {
                                                let key = String::from_utf8_lossy(key_bs).into_owned();
                                                let path = String::from_utf8_lossy(path_bs).into_owned();
                                                let index = String::from_utf8_lossy(index_bs).into_owned().parse::<isize>().map_err(|_| "JSON.ARRINSERT index is not an integer")?;
                                                let mut values_list: Vec<String> = Vec::new();
                                                for i in 4..values.len() {
                                                    if let Value::BulkString(value_bs) = &values[i] {
                                                        values_list.push(String::from_utf8_lossy(value_bs).into_owned());
                                                    } else {
                                                        return Err("JSON.ARRINSERT insert values must be bulk strings");
                                                    }
                                                }
                                                Ok(Command::JsonArrInsert { key, path, index, values: values_list })
                                            } else {
                                                return Err("JSON.ARRINSERT index must be a bulk string");
                                            }
                                        } else {
                                            Err("JSON.ARRINSERT path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.ARRINSERT key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.ARRINSERT command expects at least three value arguments after key, path and index")
                                }
                            }

                            "JSON.ARRLEN" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                String::from_utf8_lossy(path_bs).into_owned()
                                            } else {
                                                return Err("JSON.ARRLEN path must be a bulk string");
                                            }
                                        } else {
                                            ".".to_string() // Default path to root if not provided
                                        };
                                        Ok(Command::JsonArrLen { key, path })
                                    } else {
                                        Err("JSON.ARRLEN key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.ARRLEN command expects one or two arguments (key, [path])")
                                }
                            }

                            "JSON.ARRPOP" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(index_bs) = &values[2] {
                                                String::from_utf8_lossy(index_bs).into_owned().parse::<isize>().map_err(|_| "JSON.ARRPOP index is not an integer")?;
                                                String::from_utf8_lossy(index_bs).into_owned() // path is actually index here, reusing path variable for simplicity
                                            } else {
                                                return Err("JSON.ARRPOP index must be a bulk string");
                                            }
                                        } else {
                                            ".".to_string() // Default path (not used for ARRPOP index, but keeping for consistent struct)
                                        };
                                        let index = if values.len() == 3 {
                                            if let Value::BulkString(index_bs) = &values[2] {
                                                 String::from_utf8_lossy(index_bs).into_owned().parse::<isize>().ok()
                                            } else {
                                                None
                                            }

                                        } else {
                                            None // Default index to -1 if not provided
                                        };
                                        Ok(Command::JsonArrPop { key, path, index }) // Reusing path as index string for parsing simplicity
                                    } else {
                                        Err("JSON.ARRPOP key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.ARRPOP command expects one or two arguments (key, [index])")
                                }
                            }

                            "JSON.ARRTRIM" => {
                                if values.len() == 5 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(start_bs) = &values[3] {
                                                if let Value::BulkString(stop_bs) = &values[4] {
                                                    let key = String::from_utf8_lossy(key_bs).into_owned();
                                                    let path = String::from_utf8_lossy(path_bs).into_owned();
                                                    let start = String::from_utf8_lossy(start_bs).into_owned().parse::<isize>().map_err(|_| "JSON.ARRTRIM start is not an integer")?;
                                                    let stop = String::from_utf8_lossy(stop_bs).into_owned().parse::<isize>().map_err(|_| "JSON.ARRTRIM stop is not an integer")?;

                                                    Ok(Command::JsonArrTrim { key, path, start, stop })
                                                } else {
                                                    return Err("JSON.ARRTRIM stop must be a bulk string")
                                                }
                                            } else {
                                                return Err("JSON.ARRTRIM start must be a bulk string")
                                            }
                                        } else {
                                            Err("JSON.ARRTRIM path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.ARRTRIM key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.ARRTRIM command expects exactly four arguments (key, path, start, stop)")
                                }
                            }

                            // Graph Commands
                            "GRAPH.CREATE_NODE" => {
                                if values.len() >= 3 && (values.len() - 2) % 3 == 0 { // Min 3 args (command, key, node_id) and then property key value pairs
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(node_id_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let node_id = String::from_utf8_lossy(node_id_bs).into_owned();
                                            let mut properties = Vec::new();
                                            let mut i = 3;
                                            while i < values.len() {
                                                if let (Value::BulkString(prop_key_bs), Value::BulkString(prop_value_bs)) = (&values[i], &values[i+1]) {
                                                    properties.push((String::from_utf8_lossy(prop_key_bs).into_owned(), String::from_utf8_lossy(prop_value_bs).into_owned()));
                                                    i += 2;
                                                } else {
                                                    return Err("GRAPH.CREATE_NODE properties must be key value pairs as bulk strings");
                                                }
                                            }
                                            Ok(Command::GraphCreateNode { key, node_id, properties })
                                        } else {
                                            Err("GRAPH.CREATE_NODE node_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.CREATE_NODE key must be a bulk string")
                                    }

                                } else {
                                    Err("GRAPH.CREATE_NODE expects at least key and node_id, followed by property key-value pairs")
                                }
                            }

                            "GRAPH.GET_NODE" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(node_id_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let node_id = String::from_utf8_lossy(node_id_bs).into_owned();
                                            Ok(Command::GraphGetNode { key, node_id })
                                        } else {
                                            Err("GRAPH.GET_NODE node_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.GET_NODE key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.GET_NODE command expects exactly two arguments (key and node_id)")
                                }
                            }

                            "GRAPH.DELETE_NODE" => todo!(),
                            
                            "GRAPH.CREATE_EDGE" => {
                                if values.len() >= 6 && (values.len() - 5) % 3 == 0 { // Min 6 args (command, key, edge_id, source_id, target_id, relation_type) and then property key value pairs
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(edge_id_bs) = &values[2] {
                                            if let Value::BulkString(source_id_bs) = &values[3] {
                                                if let Value::BulkString(target_id_bs) = &values[4] {
                                                    if let Value::BulkString(relation_type_bs) = &values[5] {
                                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                                        let edge_id = String::from_utf8_lossy(edge_id_bs).into_owned();
                                                        let source_node_id = String::from_utf8_lossy(source_id_bs).into_owned();
                                                        let target_node_id = String::from_utf8_lossy(target_id_bs).into_owned();
                                                        let relation_type = String::from_utf8_lossy(relation_type_bs).into_owned();
                                                        let mut properties = Vec::new();
                                                        let mut i = 6;
                                                        while i < values.len() {
                                                            if let (Value::BulkString(prop_key_bs), Value::BulkString(prop_value_bs)) = (&values[i], &values[i+1]) {
                                                                properties.push((String::from_utf8_lossy(prop_key_bs).into_owned(), String::from_utf8_lossy(prop_value_bs).into_owned()));
                                                                i += 2;
                                                            } else {
                                                                return Err("GRAPH.CREATE_EDGE properties must be key value pairs as bulk strings");
                                                            }
                                                        }
                                                        Ok(Command::GraphCreateEdge { key, edge_id, source_node_id, target_node_id, relation_type, properties })
                                                    } else {
                                                        return Err("GRAPH.CREATE_EDGE relation_type must be a bulk string");
                                                    }
                                                } else {
                                                    return Err("GRAPH.CREATE_EDGE target_node_id must be a bulk string");
                                                }
                                            } else {
                                                return Err("GRAPH.CREATE_EDGE source_node_id must be a bulk string")
                                            }
                                        } else {
                                            Err("GRAPH.CREATE_EDGE edge_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.CREATE_EDGE key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.CREATE_EDGE expects at least key, edge_id, source_node_id, target_node_id, relation_type, followed by property key-value pairs")
                                }
                            }

                            "GRAPH.GET_EDGE" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(edge_id_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let edge_id = String::from_utf8_lossy(edge_id_bs).into_owned();
                                            Ok(Command::GraphGetEdge { key, edge_id })
                                        } else {
                                            Err("GRAPH.GET_EDGE edge_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.GET_EDGE key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.GET_EDGE command expects exactly two arguments (key and edge_id)")
                                }
                            }

                            "GRAPH.DELETE_EDGE" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(edge_id_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let edge_id = String::from_utf8_lossy(edge_id_bs).into_owned();
                                            Ok(Command::GraphDeleteEdge { key, edge_id })
                                        } else {
                                            Err("GRAPH.DELETE_EDGE edge_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.DELETE_EDGE key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.DELETE_EDGE command expects exactly two arguments (key and edge_id)")
                                }
                            }

                            "GRAPH.GET_NODE_PROPERTIES" => todo!(),
                            "GRAPH.SET_NODE_PROPERTY" => todo!(),
                            "GRAPH.DELETE_NODE_PROPERTY" => todo!(),
                            "GRAPH.GET_EDGE_PROPERTIES" => todo!(),
                            "GRAPH.SET_EDGE_PROPERTY" => todo!(),
                            "GRAPH.DELETE_EDGE_PROPERTY" => todo!(),
                            
                            _ => Ok(Command::Unknown(command_str.to_string())),
                        }
                    }
                    Err(_) => {
                        error!("Failed to parse command string from bulk string: {:?}", bs);
                        Err("Invalid command string encoding")
                    },
                }
            }
            _ => Err("Command must start with a bulk string"),
        }
    }
}