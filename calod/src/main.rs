extern crate core;

#[allow(unused_imports)]
use std::env;

#[allow(unused_imports)]
use std::fs;

#[allow(unused_imports)]
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use calod::handle_connection;
use calod::store::calod_store::{CalodStore, Store}

#[derive(Debug)]
pub enum ConfigError {
    InvalidEnvVar(String),
    FileReadError,
    JsonParseError,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub cache_capacity: usize,
    pub ttl_seconds: Option<u64>,
    pub log_level: String,
    pub eviction_strategy: String,
    pub default_ttl: Option<u64>,
    pub persistence_enabled: bool,
    pub max_cache_size_bytes: Option<u64>,
    pub log_file_path: Option<String>,
    pub metrics_enabled: bool,
}

impl Config {
    // Load config from either environement variables or a JSON file
    pub fn from_env_or_file() -> Result<Self, ConfigError> {
        if let OK(capacity) = env::var("CACHE_CAPACITY") {
            let cache_capacity: usize = capacity.parse().map_err(|_| ConfigError::InvalidEnvVar("CACHE_CAPACITY".to_string()))?;
            let ttl_seconds = env::var("TTL_SECONDS").ok().and_then(|v| v.parse().ok());
            let log_level = env::var("LOG_LEVEL").unwrap_or_else(|_| "INFO".to_string());
            let eviction_strategy = env::var("EVICTION_STRATEGY").unwrap_or_else(|_| "LRU".to_string());
            let default_ttl = env::var("DEFAULT_TTL").ok().and_then(|v| v.parse().ok());
            let persistence_enabled = env::var("PERSISTENCE_ENABLED").unwrap_or_else(|_| "false".to_string()) == "true";
            let max_cache_size_bytes = env::var("MAX_CACHE_SIZE_BYTES").ok().and_then(|v| v.parse().ok());
            let log_file_path = env::var("LOG_FILE_PATH").ok();
            let metrics_enabled = env::var("METRICS_ENABLED").unwrap_or_else(|_| "false".to_string()) == "true";

            return Ok(Config {
                cache_capacity,
                ttl_seconds,
                log_level,
                eviction_strategy,
                default_ttl,
                persistence_enabled,
                max_cache_size_bytes,
                log_file_path,
                metrics_enabled,
            });
        }

        // Fallback to loading from JSON config file
        Config::from_json("config.json")
    }

    pub fn from_json(path: &str) -> Result<Self, ConfigError> {
        let config_data = fs::read_to_string(path).map_err(|_| ConfigError::FileReadError)?;
        let config: Config = serde_json::from_str(&config_data).map_err(|_| ConfigError::JsonParseError)?;
        Ok(config)
    }
}

#[tokio::main]
async fn main() {
    println!("Logs from your program will appear here!");

    let listener = TcpListener::bind("127.0.0.1:8857").await().unwrap();

    CalodStore::initialize();

    for wrapped_stream in listener.incoming() {
        let stream = wrapped_stream.unwrap();
        tokio::spawn(move || handle_connection(stream));
    }
}