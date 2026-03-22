use std::{sync::Arc, time::{Duration, Instant}};

use ahash::RandomState;
use crossbeam_skiplist::SkipMap;
use dashmap::DashMap;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::store::{calod_data::DataType, error::CacheError};

/// Memory tier identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TierType {
    Hot,    // DRAM - fastest access
    Warm,   // Persistent Memory (Intel Optane, etc)
    Cold,   // NVMe storage
}

/// Access frequency tracking for data items
#[derive(Debug, Clone)]
pub struct AccessStats {
    // number of reads
    reads: u64,
    // number of writes
    writes: u64,
    // last access time
    last_access: Instant,
    // creation time
    created_at: Instant,
    // total size in bytes
    size_bytes: usize,
    // current tier location
    current_tier: TierType,
}

impl AccessStats {
    pub fn new(size_bytes: usize, tier: TierType) -> Self {
        let now = Instant::now();
        Self {
            reads: 0,
            writes: 0,
            last_access: now,
            created_at: now,
            size_bytes,
            current_tier: tier,
        }
    }

    pub fn record_read(&mut self) {
        self.reads += 1;
        self.last_access = Instant::now();
    }

    pub fn record_write(&mut self) {
        self.writes += 1;
        self.last_access = Instant::now();
    }

    pub fn hotness_score(&self) -> f64 {
        const LAMBDA: f64 = 0.5;

        let recency = 1.0 / (1.0 + self.last_access.elapsed().as_secs_f64());
        let freq = (self.reads + self.writes * 2) as f64;

        (LAMBDA * recency) + ((1.0 - LAMBDA) * freq.log2().max(0.0))
    }
}

pub type ObjectId = Uuid;

/// Memory data item stored in tiered storage
#[derive(Debug, Clone)]
pub struct TieredItem {
    // unique item identifier
    id: ObjectId,
    // key
    key: String,
    // object value
    value: DataType,
    // optional expiration time
    expires_at: Option<Instant>,
    // access statistics
    access_stats: AccessStats
}

impl TieredItem {
    pub fn new(key: String, value: DataType, tier: TierType) -> Self {
        let size = value.size();

        Self {
            id: ObjectId::new_v4(),
            key,
            value,
            expires_at: None,
            access_stats: AccessStats::new(size, tier),
        }
    }

    pub fn with_expiry(key: String, value: DataType, tier: TierType, expires_in: Duration) -> Self {
        let mut item = Self::new(key, value, tier);
        item.expires_at = Some(Instant::now() + expires_in);
        item
    }
    
    pub fn get_value(&mut self) -> &DataType {
        self.access_stats.record_read();
        &self.value
    }
    
    pub fn set_value(&mut self, value: DataType) {
        self.value = value;
        self.access_stats.record_write();
    }
    
    pub fn is_expired(&self) -> bool {
        self.expires_at.map_or(false, |expires| expires <= Instant::now())
    }
    
    pub fn size(&self) -> usize {
        self.access_stats.size_bytes
    }
    
    pub fn id(&self) -> ObjectId {
        self.id
    }
    
    pub fn key(&self) -> &str {
        &self.key
    }
    
    pub fn tier(&self) -> TierType {
        self.access_stats.current_tier
    }
    
    pub fn set_tier(&mut self, tier: TierType) {
        self.access_stats.current_tier = tier;
    }
    
    pub fn hotness_score(&self) -> f64 {
        self.access_stats.hotness_score()
    }
}

/// Tier operation message for background processing
enum TierOperation {
    Promote(ObjectId, TierType),   // Move item to a higher tier
    Demote(ObjectId, TierType),    // Move item to a lower tier
    Persist(ObjectId),             // Ensure item is persisted
    Evict(ObjectId),               // Remove item from memory
}

/// Memory tier trait - implemented by each tier type
pub trait MemoryTier: Send + Sync {
    /// Get the tier type
    fn tier_type(&self) -> TierType;
    
    /// Store an item in this tier
    fn store(&self, item: TieredItem) -> Result<(), CacheError>;
    
    /// Retrieve an item from this tier
    fn retrieve(&self, id: &ObjectId) -> Result<Option<TieredItem>, CacheError>;
    
    /// Retrieve an item by key
    fn get_by_key(&self, key: &str) -> Result<Option<TieredItem>, CacheError>;
    
    /// Remove an item from this tier
    fn remove(&self, id: &ObjectId) -> Result<Option<TieredItem>, CacheError>;
    
    /// Check if an item exists in this tier
    fn contains(&self, id: &ObjectId) -> bool;
    
    /// Get current storage usage
    fn usage(&self) -> usize;
    
    /// Get maximum capacity
    fn capacity(&self) -> usize;
    
    /// Clear all items
    fn clear(&self) -> Result<(), CacheError>;
    
    /// Get metrics for this tier
    fn metrics(&self) -> Arc<TierMetrics> { Arc::new(TierMetrics) }
}

/// Configuration for tiered memory
#[derive(Debug, Clone)]
pub struct TieredMemoryConfig {
    /// Hot tier (DRAM) size in bytes
    pub hot_tier_size: usize,
    
    /// Warm tier (Persistent Memory) size in bytes
    pub warm_tier_size: usize,
    
    /// Cold tier (NVMe) size in bytes
    pub cold_tier_size: usize,
    
    /// Path to persistent memory device or file
    pub pmem_path: String,
    
    /// Path to NVMe device or file for persistence
    pub nvme_path: String,
    
    /// WAL directory path
    pub wal_path: String,
    
    /// Whether to enable direct I/O for NVMe access
    pub direct_io: bool,
    
    /// Background promotion/demotion interval
    pub tier_check_interval_ms: u64,
    
    /// Maximum batch size for tier operations
    pub max_batch_size: usize,
    
    /// Hot tier threshold score
    pub hot_tier_threshold: f64,
    
    /// Warm tier threshold score
    pub warm_tier_threshold: f64,
}

impl Default for TieredMemoryConfig {
    fn default() -> Self {
        Self {
            hot_tier_size: 1_073_741_824,    // 1GB DRAM by default
            warm_tier_size: 10_737_418_240,  // 10GB Persistent Memory
            cold_tier_size: 107_374_182_400, // 100GB NVMe storage
            pmem_path: "/mnt/pmem0".into(),
            nvme_path: "/dev/nvme0n1".into(),
            wal_path: "/var/lib/calod/wal".into(),
            direct_io: true,
            tier_check_interval_ms: 1000,    // 1 second check interval
            max_batch_size: 128,             // Process up to 128 items per batch
            hot_tier_threshold: 10.0,        // Minimum score for hot tier
            warm_tier_threshold: 1.0,        // Minimum score for warm tier
        }
    }
}

/// Main tiered memory manager
pub struct TieredMemoryManager {
    /// Hot tier storage (DRAM)
    hot_tier: Arc<dyn MemoryTier>,
    
    /// Warm tier storage (Persistent Memory)
    warm_tier: Arc<dyn MemoryTier>,
    
    /// Cold tier storage (NVMe)
    cold_tier: Arc<dyn MemoryTier>,
    
    /// Map of object IDs to their keys for quick lookups
    id_to_key_map: DashMap<ObjectId, String, RandomState>,
    
    /// Map of keys to their object IDs
    key_to_id_map: DashMap<String, ObjectId, RandomState>,
    
    /// Access statistics sorted by hotness score
    hotness_index: Arc<SkipMap<i64, ObjectId>>,
    
    /// Configuration
    config: TieredMemoryConfig,
    
    /// Background tier operation channel
    tier_tx: mpsc::Sender<TierOperation>,
    
    /// Write-ahead log for durability
    wal: Arc<WriteAheadLog>,
}

impl TieredMemoryManager {
    pub fn new(config: TieredMemoryConfig) -> Arc<Self> {
        let (tier_tx, tier_rx) = mpsc::channel(10_000);

    let wal = Arc::new(WriteAheadLog::new(&config.wal_path, config.direct_io));

        let hot_tier = Arc::new(DRAMTier::new(config.hot_tier_size));
        let warm_tier = Arc::new(PersistentMemoryTier::new(
            &config.pmem_path,
            config.warm_tier_size
        ));
        let cold_tier = Arc::new(NVMeTier::new(
            &config.nvme_path,
            config.cold_tier_size,
            config.direct_io,
            wal.clone()
        ));

        let manager = Arc::new(Self {
            hot_tier,
            warm_tier,
            cold_tier,
            id_to_key_map: DashMap::with_capacity_and_hasher(1_000_000,RandomState::new()),
            key_to_id_map: DashMap::with_capacity_and_hasher(1_000_000, RandomState::new()),
            hotness_index: Arc::new(SkipMap::new()),
            config,
            tier_tx,
            wal,
        });

        Self::start_background_tasks(manager.clone(), tier_rx);

        manager
    }


    /// Update the hotness index with a new score
    fn update_hotness_index(&self, id: &ObjectId, score: f64) {
        // Use negative score because SkipMap is ordered ascending but we want descending
    // convert f64 score to i64 by scaling; clamp to avoid overflow
    let scaled = (-score * 1_000_000.0) as i64;
    self.hotness_index.insert(scaled, *id);
    }
    
    /// Remove an item from the hotness index
    fn remove_from_hotness_index(&self, id: &ObjectId) {
        // Since we can't easily find by value, we'd need to scan
        // In a real implementation, we'd keep a reverse index
        // For now, this is a simplification
    }
    
    /// Schedule an item for promotion to a higher tier
    fn schedule_promotion(&self, id: ObjectId, target_tier: TierType) {
        let _ = self.tier_tx.try_send(TierOperation::Promote(id, target_tier));
    }
    
    /// Schedule an item for demotion to a lower tier
    fn schedule_demotion(&self, id: ObjectId, target_tier: TierType) {
        let _ = self.tier_tx.try_send(TierOperation::Demote(id, target_tier));
    }
    
    /// Schedule an item for persistence
    fn schedule_persistence(&self, id: ObjectId) {
        let _ = self.tier_tx.try_send(TierOperation::Persist(id));
    }
    
    /// Schedule an item for eviction
    fn schedule_eviction(&self, id: ObjectId) {
        let _ = self.tier_tx.try_send(TierOperation::Evict(id));
    }

    /// Performs promotion of an item to a higher tier (Cold -> Warm -> Hot)
    fn perform_promotion(&self, id: &ObjectId, target_tier: TierType) -> Result<(), CacheError> {
        let source_iter = match target_tier {
            TierType::Hot => {
                if let Ok(Some(item)) = self.warm_tier.retrieve(id) {
                    self.hot_tier.store(item.clone())?;
                    self.warm_tier.remove(id)?;
                    Some(TierType::Warm)
                } else if let Ok(Some(item)) = self.cold_tier.retrieve(id) {
                    self.hot_tier.store(item.clone())?;
                    self.cold_tier.remove(id)?;
                    Some(TierType::Cold)
                } else {
                    None
                }
            },
            TierType::Warm => {
                if let Ok(Some(item)) = self.cold_tier.retrieve(id) {
                    self.warm_tier.store(item.clone())?;
                    self.cold_tier.remove(id)?;
                    Some(TierType::Cold)
                } else {
                    None
                }
            },
            TierType::Cold => None,
        };

        if source_iter.is_some() {
            // update tier metrics
            return Ok(());
        }

        Err(CacheError::InternalError("Item not found for promotion".into()))
    }

    /// Performs demotion of a item to a lower tier (Hot -> Warm -> Cold)
    fn perform_demotion(&self, id: &ObjectId, target_tier: TierType) -> Result<(), CacheError> {
        let source_tier = match target_tier {
            TierType::Cold => {
                if let Ok(Some(item)) = self.hot_tier.retrieve(id) {
                    self.cold_tier.store(item.clone())?;
                    self.hot_tier.remove(id)?;
                    Some(TierType::Hot)
                } else if let Ok(Some(item)) = self.warm_tier.retrieve(id) {
                    self.cold_tier.store(item.clone())?;
                    self.warm_tier.remove(id)?;
                    Some(TierType::Warm)
                } else {
                    None
                }
            }
            TierType::Warm => {
                if let Ok(Some(item)) = self.hot_tier.retrieve(id) {
                    self.warm_tier.store(item.clone())?;
                    self.hot_tier.remove(id)?;
                    Some(TierType::Hot)
                } else {
                    None
                }
            },
            TierType::Hot => None,
        };

        if source_tier.is_some() {
            return Ok(());
        }

        Err(CacheError::InternalError("Item not found for demotion".into()))
    }

    /// Persists an item to ensure durablity
    fn perform_persistence(&self, id: &ObjectId) -> Result<(), CacheError> {
    let mut item = if let Ok(Some(mut item)) = self.hot_tier.retrieve(id) {
            item
        } else if let Ok(Some(item)) = self.warm_tier.retrieve(id) {
            item
        } else if let Ok(Some(item)) = self.cold_tier.retrieve(id) {
            item
        } else {
            return Err(CacheError::KeyNotFound(format!("Item {} not found for persistence", id)));
        };

        let key = item.key().to_owned();
        let value = item.get_value().clone();

        let wal = self.wal.clone();
        tokio::spawn(async move { let _ = wal.log_set(&key, &format!("{:?}", value)).await; });
        Ok(())
    }

    fn start_background_tasks(manager: Arc<Self>, mut tier_rx: mpsc::Receiver<TierOperation>) {
        let manager_clone = manager.clone();
        tokio::spawn(async move {
            while let Some(op) = tier_rx.recv().await {
                match op {
                    TierOperation::Promote(uuid, tier_type) => {
                        if let Err(e) = manager_clone.perform_promotion(&uuid, tier_type) {
                            tracing::error!("Failed to promote object {}: {}", uuid, e);
                        }
                    },
                    TierOperation::Demote(uuid, tier_type) =>  {
                        if let Err(e) = manager_clone.perform_demotion(&uuid, tier_type) {
                            tracing::error!("Failed to demote object {}: {}", uuid, e);
                        }
                    },
                    TierOperation::Persist(uuid) => {
                        if let Err(e) = manager_clone.perform_persistence(&uuid) {
                            tracing::error!("Failed to persist object {}: {}", uuid, e);
                        }
                    },
                    TierOperation::Evict(uuid) => {
                        // eviction placeholder
                        let _ = manager_clone.perform_eviction(&uuid);
                    },
                }
            }
        });

        let manager_clone = manager.clone();
        let config = manager.config.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(config.tier_check_interval_ms));

            loop {
                interval.tick().await;
                manager_clone.rebalance_tiers();
            }
        });
    }

    /// Get an item from the tiered storage
    pub async fn get(&self, key: &str) -> Result<Option<DataType>, CacheError> {
        if let Some(id_entry) = self.key_to_id_map.get(key) {
            let id = *id_entry.value();

            if let Ok(Some(mut item)) = self.hot_tier.retrieve(&id) {
                let value = item.get_value().clone();
                self.update_hotness_index(&id, item.hotness_score());
                return Ok(Some(value));
            }

            if let Ok(Some(mut item)) = self.warm_tier.retrieve(&id) {
                let value = item.get_value().clone();
                let score = item.hotness_score();

                self.update_hotness_index(&id, score);

                if score > self.config.hot_tier_threshold {
                    self.schedule_promotion(id, TierType::Hot);
                }

                return Ok(Some(value));
            }

            if let Ok(Some(mut item)) = self.cold_tier.retrieve(&id) {
                let value = item.get_value().clone();
                let score = item.hotness_score();

                self.update_hotness_index(&id, score);

                if score > self.config.hot_tier_threshold {
                    self.schedule_promotion(id, TierType::Hot);
                } else if score > self.config.warm_tier_threshold {
                    self.schedule_promotion(id, TierType::Warm);
                }

                return Ok(Some(value));
            }

            self.key_to_id_map.remove(key);
            self.id_to_key_map.remove(&id);

            self.remove_from_hotness_index(&id)
        }
        
        Ok(None)
    }
}

// ---- Placeholder stubs to satisfy references ----
impl TieredMemoryManager {
    fn perform_eviction(&self, _id: &ObjectId) -> Result<(), CacheError> { Ok(()) }
    fn rebalance_tiers(&self) { /* scan & schedule promotions/demotions simplified */ }
}

pub struct TierMetrics;
pub struct WriteAheadLog; impl WriteAheadLog { pub fn new(_p: &str, _d: bool) -> Self { Self } pub async fn log_set(&self, _k: &str, _v: &str) -> Result<(), ()>{ Ok(()) } }
pub struct DRAMTier; impl DRAMTier { pub fn new(_s: usize) -> Self { Self } }
pub struct PersistentMemoryTier; impl PersistentMemoryTier { pub fn new(_p: &str, _s: usize) -> Self { Self } }
pub struct NVMeTier; impl NVMeTier { pub fn new(_p: &str, _s: usize, _d: bool, _wal: Arc<WriteAheadLog>) -> Self { Self } }

impl MemoryTier for DRAMTier {
    fn tier_type(&self) -> TierType { TierType::Hot }
    fn store(&self, _item: TieredItem) -> Result<(), CacheError> { Ok(()) }
    fn retrieve(&self, _id: &ObjectId) -> Result<Option<TieredItem>, CacheError> { Ok(None) }
    fn get_by_key(&self, _key: &str) -> Result<Option<TieredItem>, CacheError> { Ok(None) }
    fn remove(&self, _id: &ObjectId) -> Result<Option<TieredItem>, CacheError> { Ok(None) }
    fn contains(&self, _id: &ObjectId) -> bool { false }
    fn usage(&self) -> usize { 0 }
    fn capacity(&self) -> usize { 0 }
    fn clear(&self) -> Result<(), CacheError> { Ok(()) }
}
impl MemoryTier for PersistentMemoryTier { fn tier_type(&self) -> TierType { TierType::Warm } fn store(&self,_:TieredItem)->Result<(),CacheError>{Ok(())} fn retrieve(&self,_:&ObjectId)->Result<Option<TieredItem>,CacheError>{Ok(None)} fn get_by_key(&self,_:&str)->Result<Option<TieredItem>,CacheError>{Ok(None)} fn remove(&self,_:&ObjectId)->Result<Option<TieredItem>,CacheError>{Ok(None)} fn contains(&self,_:&ObjectId)->bool{false} fn usage(&self)->usize{0} fn capacity(&self)->usize{0} fn clear(&self)->Result<(),CacheError>{Ok(())} }
impl MemoryTier for NVMeTier { fn tier_type(&self) -> TierType { TierType::Cold } fn store(&self,_:TieredItem)->Result<(),CacheError>{Ok(())} fn retrieve(&self,_:&ObjectId)->Result<Option<TieredItem>,CacheError>{Ok(None)} fn get_by_key(&self,_:&str)->Result<Option<TieredItem>,CacheError>{Ok(None)} fn remove(&self,_:&ObjectId)->Result<Option<TieredItem>,CacheError>{Ok(None)} fn contains(&self,_:&ObjectId)->bool{false} fn usage(&self)->usize{0} fn capacity(&self)->usize{0} fn clear(&self)->Result<(),CacheError>{Ok(())} }