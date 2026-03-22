use std::{net::SocketAddr, sync::atomic::{AtomicU64, Ordering}, time::{Duration, Instant}};


#[derive(Debug, Clone, PartialEq)]
pub enum NodeRole {
    Leader,
    Follower,
    Observer,
}

/// Represents a node in the cluster
#[derive(Debug)]
pub struct ClusterNode {
    /// node id
    id: String,

    /// address of the node
    addr: SocketAddr,

    /// role of the node
    role: NodeRole,

    /// last ping timestamp
    last_ping: Instant,

    /// last successfuly ping time
    last_pong: Instant,

    /// ping latency in millis
    ping_latency: AtomicU64,

    /// node reachable ?
    reachable: bool,
}

impl ClusterNode {
    /// create a new cluster node
    pub fn new(id: String, addr: SocketAddr, role: NodeRole) -> Self {
        let now = Instant::now();

        Self {
            id,
            addr,
            role,
            last_ping: now,
            last_pong: now,
            ping_latency: AtomicU64::new(0),
            reachable: true,
        }
    }

    /// Get the node id
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the node address
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Get the node role
    pub fn role(&self) -> &NodeRole {
        &self.role
    }

    /// Set the node role
    pub fn set_role(&mut self, role: NodeRole) {
        self.role = role;
    }

    /// Record a ping sent
    pub fn ping_sent(&mut self) {
        self.last_ping = Instant::now();
    }

    /// Record a pong received
    pub fn pong_received(&mut self) {
        self.last_pong = Instant::now();
        let latency = self.last_pong.duration_since(self.last_ping).as_millis() as u64;
        self.ping_latency.store(latency, Ordering::Relaxed);
        self.reachable = true;
    }

    /// Mark the node as unreachable
    pub fn mark_unreachable(&mut self) {
        self.reachable = false;
    }

    /// Check of the node is reachable
    pub fn is_reachable(&self) -> bool {
        self.reachable
    }

    /// Get the ping latency in milliseconds
    pub fn ping_latency(&self) -> u64 {
        self.ping_latency.load(Ordering::Relaxed)
    }

    /// Get the time since last successful ping
    pub fn time_since_last_pong(&self) -> Duration {
        self.last_pong.elapsed()
    }


}