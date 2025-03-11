use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Connection-specific metrics tracking
pub struct ConnectionMetrics {
    bytes_received: AtomicU64,
    bytes_sent: AtomicU64,
    commands_processed: AtomicU64,
    total_execution_time_micros: AtomicU64,
}

impl ConnectionMetrics {
    pub fn new() -> Self {
        Self {
            bytes_received: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            commands_processed: AtomicU64::new(0),
            total_execution_time_micros: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn bytes_received(&self, bytes: u64) {
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn bytes_sent(&self, bytes: u64) {
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }
    
    #[inline]
    pub fn command_received(&self) {
        self.commands_processed.fetch_add(1, Ordering::Relaxed);
    }
    
    #[inline]
    pub fn command_executed(&self, duration: Duration) {
        let micros = duration.as_micros() as u64;
        self.total_execution_time_micros.fetch_add(micros, Ordering::Relaxed);
    }
    
    #[inline]
    pub fn connection_established(&self) {
        // Nothing to track at connection level
    }
    
    #[inline]
    pub fn connection_closed(&self) {
        // Nothing to track at connection level
    }

}

/// Server-wide metrics aggregator
pub struct MetricsAggregator {
    total_connections: AtomicU64,
    current_connections: AtomicU64,
    total_commands: AtomicU64,
}

impl MetricsAggregator {
    pub fn new() -> Self {
        Self {
            total_connections: AtomicU64::new(0),
            current_connections: AtomicU64::new(0),
            total_commands: AtomicU64::new(0),
        }
    }
    
    #[inline]
    pub fn new_connection_metrics(&self) -> Arc<ConnectionMetrics> {
        Arc::new(ConnectionMetrics::new())
    }
    
    #[inline]
    pub fn connections_total_inc(&self) {
        self.total_connections.fetch_add(1, Ordering::Relaxed);
        self.current_connections.fetch_add(1, Ordering::Relaxed);
    }
    
    #[inline]
    pub fn connections_total_dec(&self) {
        self.current_connections.fetch_sub(1, Ordering::Relaxed);
    }
    
    #[inline]
    pub fn get_total_connections(&self) -> u64 {
        self.total_connections.load(Ordering::Relaxed)
    }
    
    #[inline]
    pub fn get_current_connections(&self) -> u64 {
        self.current_connections.load(Ordering::Relaxed)
    }
}