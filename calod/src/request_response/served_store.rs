use std::sync::Arc;

use tracing::{debug, error, info};

use crate::store::{calod_store::{CacheError, ShardedStore}, config::CacheConfig};
use crate::store::calod_data::DataType;
use super::command::Command;

pub struct ServedCalodStore {
    store: Arc<ShardedStore>,
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

    fn wrap_response(&self, data: DataType) -> Vec<u8> {
        match data {
            DataType::String(s) => {
                let response = format!("${}\r\n{}\r\n", s.len(), s).into_bytes();
                debug!("Wrapped response: {:?}", String::from_utf8_lossy(&response));
                response
            },
            DataType::List(items) => {
                let mut resp = format!("*{}\r\n", items.len()).into_bytes();
                for item in items {
                    resp.extend(format!("${}\r\n{}\r\n", item.len(), item).as_bytes());
                }
                debug!("Wrapped response: {:?}", String::from_utf8_lossy(&resp));
                resp
            }
            DataType::Set(items) => {
                let mut resp = format!("*{}\r\n", items.len()).into_bytes();
                for item in items {
                    resp.extend(format!("${}\r\n{}\r\n", item.len(), item).as_bytes());
                }
                debug!("Wrapped response: {:?}", String::from_utf8_lossy(&resp));
                resp
            },
            DataType::Hash(map) => {
                let mut resp = format!("*{}\r\n", map.len() * 2).into_bytes();
                for (k, v) in map {
                    resp.extend(format!("${}\r\n{}\r\n", k.len(), k).as_bytes());
                    resp.extend(format!("${}\r\n{}\r\n", v.len(), v).as_bytes());
                }
                debug!("Wrapped response: {:?}", String::from_utf8_lossy(&resp));
                resp
            },
            DataType::Object { data, type_info } => {
                error!("DataType::Object not implemented yet");
                b"-ERR Object type not implemented\r\n".to_vec()
            },
        }
    }

    pub async fn execute(&self, cmd: Command) -> Vec<u8> {
        info!("Executing command: {:?}", cmd);
        match cmd {
            Command::Info { section } => self.info(Some(&section)).await,
            Command::Get { key } => {
                match self.store.get(&key).await {
                    Ok(value) => self.wrap_response(value),
                    Err(CacheError::KeyNotFound(_)) => b"$-1\r\n".to_vec(),
                    Err(e) => format!("-ERR {}\r\n", e).into_bytes(),
                }
            }
            Command::Set { key, value } => {
                let data_type = DataType::String(value);
                let _ = self.store.set(key, data_type, None).await;
                b"+OK\r\n".to_vec()
            },
            Command::Exists { key } => {
                match self.store.exists(&key).await {
                    true => b":1\r\n".to_vec(),
                    false => b":1\r\n".to_vec(),
                }
            }
            Command::Del { key } => {
                let result = self.store.delete(&key).await;
                if result { b":1\r\n".to_vec() } else { b":0\r\n".to_vec() } 
            }
            Command::Keys { pattern } => {
                let keys = self.store.keys(&pattern).await;
                let mut response = format!("*{}\r\n", keys.len());
                for key in keys {
                    response.push_str(&format!("${}\r\n{}\r\n", key.len(), key));
                }
                response.into_bytes()
            }
            _ => {
                error!("Command not implemented: {:?}", cmd);
                b"-ERR Not implemented\r\n".to_vec()
            }
        }
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
}