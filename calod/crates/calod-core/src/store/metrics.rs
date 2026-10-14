use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde_derive::Serialize;

#[derive(Debug, Default, Serialize)]
pub struct CalodMetrics {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub evictions: AtomicU64,
    pub writes: AtomicU64,
    pub reads: AtomicU64,
    pub avg_read_latency: AtomicU64,
    pub avg_write_latency: AtomicU64,
    pub total_data_size: AtomicU64,
}

impl CalodMetrics {
    pub fn record_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_eviction(&self) {
        self.evictions.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_read(&self, latency: Duration) {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.avg_read_latency
            .fetch_add(latency.as_micros() as u64, Ordering::Relaxed);
    }

    pub fn record_write(&self, latency: Duration) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.avg_write_latency
            .fetch_add(latency.as_micros() as u64, Ordering::Relaxed);
    }

    pub fn report(&self) -> String {
        let reads = self.reads.load(Ordering::Relaxed);
        let writes = self.writes.load(Ordering::Relaxed);

        let avg_read = self
            .avg_read_latency
            .load(Ordering::Relaxed)
            .checked_div(reads)
            .unwrap_or(0);

        let avg_write = self
            .avg_write_latency
            .load(Ordering::Relaxed)
            .checked_div(writes)
            .unwrap_or(0);

        format!(
            "Cache Metrics:\r\n\
             Hits: {}\r\nMisses: {}\r\nEvictions: {}\r\nReads: {}\r\nWrites: {}\r\n\
             Avg Read Latency: {}μs\r\nAvg Write Latency: {}μs\r\nTotal Size: {} bytes\r\n",
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
            self.evictions.load(Ordering::Relaxed),
            reads,
            writes,
            avg_read,
            avg_write,
            self.total_data_size.load(Ordering::Relaxed)
        )
    }

    #[inline]
    pub fn total_data_size(&self) -> u64 {
        self.total_data_size.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::CalodMetrics;

    #[test]
    fn report_uses_resp_info_line_endings() {
        let report = CalodMetrics::default().report();

        assert!(report.contains("Total Size: 0 bytes\r\n"));
        assert!(!report.contains("bytestotal_connections_received"));
    }
}
