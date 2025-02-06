use std::time::Duration;

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub shards: usize,
    pub capacity: usize,
    pub eviction_sample_size: usize,
    pub metrics_enabled: bool,
    pub persistence_interval: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            shards: 16,
            capacity: 10_000,
            eviction_sample_size: 5,
            metrics_enabled: true,
            persistence_interval: Duration::from_secs(60),
        }
    }
}