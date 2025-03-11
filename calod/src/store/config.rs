use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf};

/// CalodDB configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalodConfig {
    // Basic server config
    pub listen_addr: SocketAddr,
    pub max_connections: usize,
    pub log_level: String,
    pub metrics_enabled: bool,
    
    // Cache configuration
    pub shards: usize,
    pub shard_capacity: usize, 
    pub max_memory_mb: usize,
    pub eviction_policy: EvictionPolicy,
    
    // Persistence configuration
    pub persistence: PersistenceConfig,
    
    // Cluster configuration
    pub cluster: ClusterConfig,
    
    // Security configuration
    pub security: SecurityConfig,
    
    // Advanced configuration
    pub advanced: AdvancedConfig,
}

impl Default for CalodConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:6379".parse().unwrap(),
            max_connections: 10000,
            log_level: "info".to_string(),
            metrics_enabled: true,
            shards: num_cpus::get(),
            shard_capacity: 1000000,
            max_memory_mb: 1024,
            eviction_policy: EvictionPolicy::LruApproximated,
            persistence: PersistenceConfig::default(),
            cluster: ClusterConfig::default(),
            security: SecurityConfig::default(),
            advanced: AdvancedConfig::default(),
        }
    }
}

/// Cache eviction policies
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvictionPolicy {
    /// No eviction, return errors when memory limit is reached
    NoEviction,
    /// Least recently used keys are removed first
    LruExact,
    /// Approximated LRU using sampling for better performance
    LruApproximated,
    /// Least frequently used keys are removed first
    Lfu,
    /// Random keys are removed
    Random,
    /// Keys closest to expiration are removed first
    Ttl,
}

/// Persistence configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceConfig {
    /// Enable RDB-style persistence
    pub enable_snapshot: bool,
    /// Directory for storing snapshot files
    pub snapshot_dir: PathBuf,
    /// How often to create snapshots (in seconds)
    pub snapshot_interval_secs: u64,
    /// Minimum number of changes to trigger snapshot
    pub snapshot_min_changes: u64,
    
    /// Enable AOF-style persistence
    pub enable_aof: bool,
    /// Directory for storing AOF files
    pub aof_dir: PathBuf,
    /// How often to fsync AOF (always, everysec, no)
    pub aof_fsync_strategy: FsyncStrategy,
    /// Enable AOF rewriting to optimize size
    pub aof_rewrite_enabled: bool,
    /// Percentage of growth to trigger AOF rewrite
    pub aof_rewrite_percentage: u8,
    /// Minimum size in MB to trigger AOF rewrite
    pub aof_rewrite_min_size_mb: u64,
}

/// Fsync strategies for AOF persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsyncStrategy {
    /// Fsync after every write
    Always,
    /// Fsync every second
    Everysec,
    /// Let OS decide when to sync
    No,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            enable_snapshot: true,
            snapshot_dir: PathBuf::from("./data"),
            snapshot_interval_secs: 3600,
            snapshot_min_changes: 10000,
            
            enable_aof: false,
            aof_dir: PathBuf::from("./data"),
            aof_fsync_strategy: FsyncStrategy::Everysec,
            aof_rewrite_enabled: true,
            aof_rewrite_percentage: 100,
            aof_rewrite_min_size_mb: 64,
        }
    }
}

/// Cluster configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// Enable cluster mode
    pub enabled: bool,
    /// Cluster name
    pub name: String,
    /// Node ID (must be unique)
    pub node_id: String,
    /// Known node addresses to connect to on startup
    pub seed_nodes: Vec<String>,
    /// Number of replicas for each shard
    pub replicas: u8,
    /// How often to check peer health
    pub health_check_interval_ms: u64,
    /// Timeout for peer connections
    pub peer_timeout_ms: u64,
    /// Timeout for leader election
    pub election_timeout_ms: u64,
    /// Enable auto-discovery of nodes
    pub auto_discovery: bool,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            name: "calod-cluster".to_string(),
            node_id: uuid::Uuid::new_v4().to_string(),
            seed_nodes: Vec::new(),
            replicas: 1,
            health_check_interval_ms: 1000,
            peer_timeout_ms: 3000,
            election_timeout_ms: 5000,
            auto_discovery: true,
        }
    }
}

/// Security configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Authentication password (none if empty)
    pub password: Option<String>,
    /// Path to ACL configuration file
    pub acl_file: Option<PathBuf>,
    /// Enable TLS
    pub tls_enabled: bool,
    /// Path to TLS certificate
    pub tls_cert_path: Option<PathBuf>,
    /// Path to TLS private key
    pub tls_key_path: Option<PathBuf>,
    /// Path to CA certificate for client authentication
    pub tls_ca_path: Option<PathBuf>,
    /// Require client certificate authentication
    pub tls_client_auth: bool,
    /// Enable data-at-rest encryption
    pub encryption_enabled: bool,
    /// Encryption key (autogenerated if not provided)
    pub encryption_key: Option<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            password: None,
            acl_file: None,
            tls_enabled: false,
            tls_cert_path: None,
            tls_key_path: None,
            tls_ca_path: None,
            tls_client_auth: false,
            encryption_enabled: false,
            encryption_key: None,
        }
    }
}

/// Advanced configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvancedConfig {
    /// Max client query buffer size in bytes
    pub max_client_query_buffer_size: usize,
    /// Max command execution time in ms
    pub max_command_execution_time_ms: u64,
    /// Enable debug commands
    pub enable_debug_commands: bool,
    /// Enable memory profiling
    pub enable_memory_profiling: bool,
    /// Command statistics collection
    pub collect_command_stats: bool,
    /// Enable slow log
    pub slow_log_enabled: bool,
    /// Slow log threshold in microseconds
    pub slow_log_threshold_micros: u64,
    /// Slow log max length
    pub slow_log_max_len: usize,
    /// Listen backlog size
    pub tcp_backlog: u32,
    /// TCP keepalive interval
    pub tcp_keepalive_secs: u64,
    /// Number of I/O threads
    pub io_threads: usize,
    /// Background job queue size
    pub background_job_queue_size: usize,
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        Self {
            max_client_query_buffer_size: 1024 * 1024, // 1MB
            max_command_execution_time_ms: 5000,
            enable_debug_commands: false,
            enable_memory_profiling: false,
            collect_command_stats: true,
            slow_log_enabled: true,
            slow_log_threshold_micros: 10000, // 10ms
            slow_log_max_len: 128,
            tcp_backlog: 511,
            tcp_keepalive_secs: 300,
            io_threads: num_cpus::get(),
            background_job_queue_size: 1024,
        }
    }
}