use std::{sync::atomic::{AtomicU64, Ordering}, time::Duration};

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
        self.avg_read_latency.fetch_add(latency.as_micros() as u64, Ordering::Relaxed);
    }

    pub fn record_write(&self, latency: Duration) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.avg_write_latency.fetch_add(latency.as_micros() as u64, Ordering::Relaxed);
    }

    pub fn report(&self) -> String {
        let reads = self.reads.load(Ordering::Relaxed);
        let writes = self.writes.load(Ordering::Relaxed);

        let avg_read = if reads > 0 {
            self.avg_read_latency.load(Ordering::Relaxed) / reads
        } else { 0 };

        let avg_write = if writes > 0 {
            self.avg_write_latency.load(Ordering::Relaxed) / writes
        } else { 0 };

        format!(
            "Cache Metrics:\n\
             Hits: {}\nMisses: {}\nEvictions: {}\nReads: {}\nWrites: {}\n\
             Avg Read Latency: {}μs\nAvg Write Latency: {}μs\nTotal Size: {} bytes",
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
}