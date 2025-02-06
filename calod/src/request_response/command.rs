use serde_json;
use tracing::error;
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
pub enum Command {
    // Generic
    Ping {},
    Keys {
        pattern: String,
    },
    Exists {
        key: String,
    },
    Expire {
        key: String,
        seconds: u64,
    },
    Ttl {
        key: String,
    },
    Persist {
        key: String,
    },
    Scan {
        cursor: u64,
        pattern: String,
        count: u64,
    },
    Del {
        key: String,
    },
    Info {
        section: String,
    },

    // strings/numbers
    Set {
        key: String,
        value: String,
    },
    Get {
        key: String,
    },
    MGet {
        key: String,
    },
    Incr {
        key: String,
    },
    Decr {
        key: String,
    },

    // hashes
    HSet {
        key: String,
        field: String,
        value: String,
    },
    HGet {
        key: String,
        field: String,
    },
    HGetAll {
        key: String,
    },
    HDel {
        key: String,
        field: String,
    },
    HMGet {
        key: String,
        field: String,
    },

    // Sets
    XAdd {
        key: String,
        member: String,
    },
    XRead {
        key: String,
    },
    XDel {
        key: String,
    },
    XTrim {
        key: String,
        maxlen: u64,
    },
    XRem {
        key: String,
        member: String,
    },

    // sorted sets
    ZAdd {
        key: String,
        score: f64,
        memeber: String,
    },
    ZRange {
        key: String,
        start: isize,
        stop: isize,
    },

    // Lists
    ListLPush {
        key: String,
        value: String,
    },
    ListRPush {
        key: String,
        value: String,
    },
    ListLRange {
        key: String,
        start: isize,
        end: isize,
    },

    // Shared between Lists and Streams
    LLen {
        key: String,
    },
    LPop {
        key: String,
    },
    RPop {
        key: String,
    },

    // Streams
    StreamXAdd {
        key: String,
        value: String,
    },
    StreamXRead {
        key: String,
        value: String,
    },
    StreamXRange {
        key: String,
        start: isize,
        end: isize,
    },

    JsonSet {
        key: String,
        path: String,
        value: serde_json::Value,
    },
    JsonGet {
        key: String,
        path: String,
    },
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
                            "PING" => {
                                if values.len() == 1 {
                                    Ok(Command::Ping {  })
                                } else {
                                    Err("PING command expectes no arguments")
                                }
                            }
                            "SET" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(value_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let value = String::from_utf8_lossy(value_bs).into_owned();
                                            Ok(Command::Set { key, value })
                                        } else {
                                            Err("SET command value must be a bulk string")
                                        }
                                    } else {
                                        Err("SET command key must be a bulk string")
                                    }
                                } else {
                                    Err("SET command expects exactly two arguments (key and value)")
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
                            "INFO" => {
                                if values.len() <= 2 {
                                    let section = if values.len() == 2 {
                                        if let Value::BulkString(section_bs) = &values[1] {
                                            String::from_utf8_lossy(section_bs).into_owned()
                                        } else {
                                            return Err("INFO command section must be a bulk string");
                                        }
                                    } else {
                                        "default".to_string() // Default section if none provided
                                    };
                                    Ok(Command::Info { section })
                                } else {
                                    Err("INFO command expects at most one argument (section)")
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