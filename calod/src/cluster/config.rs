use std::time::Duration;

/// Core RDMA & cluster configuration.
/// This is an initial scaffold – fields will grow as functionality expands.
#[derive(Debug, Clone)]
pub struct RdmaConfig {
	/// Name of the RDMA device (e.g. mlx5_0). If None / empty => auto-pick first.
	pub device_name: Option<String>,
	/// Maximum concurrent RDMA / logical connections.
	pub max_connections: usize,
	/// Queue pair depth (send/recv). We start conservative.
	pub qp_depth: u32,
	/// Maximum inline data threshold (small writes can be inlined for latency).
	pub inline_threshold: usize,
	/// Total number of logical shards across the cluster (hash range partitioning).
	pub total_shards: usize,
	/// Replication factor (primary + (rf-1) replicas).
	pub replication_factor: usize,
	/// Size (bytes) of global value arena per node (registered / pinned memory).
	pub value_arena_bytes: usize,
	/// Number of directory entries (key metadata slots) per node.
	pub directory_entries: usize,
	/// Authentication token expiry seconds.
	pub auth_token_expiry_secs: u64,
	/// Key rotation interval seconds (encryption / HMAC keys).
	pub key_rotation_interval_secs: u64,
	/// Default remote read permission.
	pub default_allow_remote_read: bool,
	/// Default remote write permission.
	pub default_allow_remote_write: bool,
	/// Default remote atomic ops permission.
	pub default_allow_remote_atomic: bool,
	/// Allow direct remote SET protocol (else fallback to RPC style proxying).
	pub allow_remote_writes: bool,
	/// Gossip / heartbeat interval.
	pub heartbeat_interval: Duration,
	/// Failure detection timeout (mark suspect if no pong within).
	pub heartbeat_timeout: Duration,
	/// Rebalance throttle interval.
	pub rebalance_interval: Duration,
}

impl Default for RdmaConfig {
	fn default() -> Self {
		Self {
			device_name: None,
			max_connections: 1024,
			qp_depth: 256,
			inline_threshold: 256,
			total_shards: 128,
			replication_factor: 1,
			value_arena_bytes: 512 * 1024 * 1024, // 512MB
			directory_entries: 1 << 20, // ~1M entries
			auth_token_expiry_secs: 3600,
			key_rotation_interval_secs: 6 * 3600,
			default_allow_remote_read: true,
			default_allow_remote_write: false,
			default_allow_remote_atomic: false,
			allow_remote_writes: false,
			heartbeat_interval: Duration::from_millis(750),
			heartbeat_timeout: Duration::from_secs(5),
			rebalance_interval: Duration::from_secs(30),
		}
	}
}

impl RdmaConfig {
	/// Validate core invariants; returns false if obviously misconfigured.
	pub fn validate(&self) -> Result<(), String> {
		if self.replication_factor == 0 { return Err("replication_factor must be >= 1".into()); }
		if self.total_shards == 0 { return Err("total_shards must be >= 1".into()); }
		if self.directory_entries == 0 { return Err("directory_entries must be >= 1".into()); }
		if self.value_arena_bytes < 8 * 1024 * 1024 { return Err("value_arena_bytes too small".into()); }
		if self.replication_factor > 8 { /* soft guard */ }
		Ok(())
	}
}