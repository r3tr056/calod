use std::{collections::HashMap, net::SocketAddr, sync::{Arc, atomic::{AtomicU64, Ordering}}, time::{Duration, Instant}};

use dashmap::DashMap;
use tokio::{sync::{RwLock, broadcast, mpsc, Mutex}, task::JoinHandle};
use tracing::{info, warn};

use super::{config::RdmaConfig, node::{ClusterNode, NodeRole}};

/// Cluster-wide unique identifier type (string for now; could be ULID/UUID).
pub type NodeId = String;

/// Mapping from shard id -> (primary, replicas)
#[derive(Debug, Clone)]
pub struct ShardAssignment {
    pub primary: NodeId,
    pub replicas: Vec<NodeId>,
    pub epoch: u64,
}

/// Cluster state snapshot
#[derive(Debug, Clone)]
pub struct ClusterView {
    pub epoch: u64,
    pub nodes: HashMap<NodeId, SocketAddr>,
    pub shards: HashMap<usize, ShardAssignment>,
}

/// Commands emitted internally for maintenance / membership changes
#[derive(Debug)]
enum ClusterCommand {
    HeartbeatTick,
    RebalanceTick,
    NodeJoin { id: NodeId, addr: SocketAddr },
    NodeLeave { id: NodeId },
}

/// Public events subscribers can listen to (e.g. reconfigure routers)
#[derive(Debug, Clone)]
pub enum ClusterEvent {
    ViewUpdated(ClusterView),
}

/// Cluster manager – single authority instance per node; leader performs assignment.
pub struct ClusterManager {
    /// Config
    config: Arc<RdmaConfig>,
    /// Local node id
    local_id: NodeId,
    /// Local address
    local_addr: SocketAddr,
    /// Known nodes (includes self)
    nodes: DashMap<NodeId, ClusterNode>,
    /// Shard assignments
    shard_map: RwLock<HashMap<usize, ShardAssignment>>,
    /// Epoch (monotonic config version)
    epoch: AtomicU64,
    /// Broadcast for view changes
    event_tx: broadcast::Sender<ClusterEvent>,
    /// Internal command channel
    cmd_tx: mpsc::Sender<ClusterCommand>,
    /// Worker handle
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl ClusterManager {
    pub fn new(local_id: NodeId, local_addr: SocketAddr, config: Arc<RdmaConfig>) -> Arc<Self> {
        let (event_tx, _rx) = broadcast::channel(64);
        let (cmd_tx, cmd_rx) = mpsc::channel(256);

        let mgr = Arc::new(Self {
            config,
            local_id: local_id.clone(),
            local_addr,
            nodes: DashMap::new(),
            shard_map: RwLock::new(HashMap::new()),
            epoch: AtomicU64::new(1),
            event_tx,
            cmd_tx,
            worker: Mutex::new(None),
        });
        mgr.nodes.insert(local_id.clone(), ClusterNode::new(local_id, local_addr, NodeRole::Leader));
        mgr.spawn_worker(cmd_rx);
        mgr
    }

    fn spawn_worker(self: &Arc<Self>, mut cmd_rx: mpsc::Receiver<ClusterCommand>) {
        let this = Arc::clone(self);
        let heartbeat_interval = this.config.heartbeat_interval;
        let rebalance_interval = this.config.rebalance_interval;
        let handle = tokio::spawn(async move {
            let mut last_hb = Instant::now();
            let mut last_reb = Instant::now();
            loop {
                tokio::select! {
                    Some(cmd) = cmd_rx.recv() => {
                        match cmd { ClusterCommand::HeartbeatTick => { /* explicit tick */ }, ClusterCommand::RebalanceTick => { this.rebalance_if_needed().await; }, ClusterCommand::NodeJoin { id, addr } => { this.add_node(id, addr).await; }, ClusterCommand::NodeLeave { id } => { this.remove_node(&id).await; }, }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if last_hb.elapsed() >= heartbeat_interval { this.heartbeat_round().await; last_hb = Instant::now(); }
                        if last_reb.elapsed() >= rebalance_interval { this.rebalance_if_needed().await; last_reb = Instant::now(); }
                    }
                }
            }
        });
        info!("cluster manager worker started");
        // store handle
        if let Ok(mut guard) = self.worker.try_lock() { *guard = Some(handle); }
    }

    async fn heartbeat_round(&self) {
        // For now: mark unreachable if ping latency > timeout placeholder.
        let timeout = self.config.heartbeat_timeout;
        let mut dead = Vec::new();
        for kv in self.nodes.iter() { if kv.id() == self.local_id { continue; } if kv.time_since_last_pong() > timeout { dead.push(kv.id().to_string()); } }
        for id in dead { warn!(node=%id, "marking node as dead"); self.nodes.remove(&id); }
        // Could send heartbeats here (TCP control plane) – left as future work.
    }

    async fn rebalance_if_needed(&self) {
        // Simple strategy: recompute full shard map if node count * replication factor changed vs last assignment.
        let node_ids: Vec<NodeId> = self.nodes.iter().map(|n| n.id().to_string()).collect();
        if node_ids.is_empty() { return; }
        // Build deterministic ordering
        let mut sorted = node_ids.clone();
        sorted.sort();
        let rf = self.config.replication_factor.min(sorted.len());
        let mut new_map = HashMap::new();
        for shard_id in 0..self.config.total_shards { let primary_idx = shard_id % sorted.len(); let primary = sorted[primary_idx].clone(); let mut replicas = Vec::new(); for i in 1..rf { replicas.push(sorted[(primary_idx + i) % sorted.len()].clone()); } new_map.insert(shard_id, ShardAssignment { primary, replicas, epoch: self.epoch.load(Ordering::Relaxed)+1 }); }
        *self.shard_map.write().await = new_map;
        let new_epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.broadcast_view(new_epoch).await;
    }

    async fn add_node(&self, id: NodeId, addr: SocketAddr) {
        if self.nodes.contains_key(&id) { return; }
        self.nodes.insert(id.clone(), ClusterNode::new(id, addr, NodeRole::Follower));
        self.rebalance_if_needed().await;
    }

    async fn remove_node(&self, id: &str) {
        self.nodes.remove(id);
        self.rebalance_if_needed().await;
    }

    async fn broadcast_view(&self, epoch: u64) {
        let mut nodes_map = HashMap::new();
        for n in self.nodes.iter() { nodes_map.insert(n.id().to_string(), n.addr()); }
        let shard_map = self.shard_map.read().await;
        let mut shards_copy = HashMap::new();
        for (k,v) in shard_map.iter() { shards_copy.insert(*k, v.clone()); }
        let view = ClusterView { epoch, nodes: nodes_map, shards: shards_copy };
        let _ = self.event_tx.send(ClusterEvent::ViewUpdated(view));
    }

    /// Subscribe to cluster events.
    pub fn subscribe(&self) -> broadcast::Receiver<ClusterEvent> { self.event_tx.subscribe() }

    /// Lookup shard assignment.
    pub async fn shard_assignment(&self, shard_id: usize) -> Option<ShardAssignment> { self.shard_map.read().await.get(&shard_id).cloned() }
}
