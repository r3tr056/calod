use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use dashmap::DashMap;
use ahash::AHasher;
use std::hash::{Hash, Hasher};

/// A packed directory entry describing the physical location of a value.
/// This will later allow remote nodes (with proper authorization) to perform
/// zero-copy RDMA reads once memory regions are globally registered.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DirectoryEntry {
    /// 64-bit hash of the user key (AHash currently). Collisions are handled at higher layer.
    pub key_hash: u64,
    /// Shard owning the authoritative copy (primary) for this key.
    pub shard_id: u32,
    /// Length of the value in bytes (for bounds checking / remote reads).
    pub value_len: u32,
    /// Flags (bitfield) – reserved for future (data type, compression, ttl present, etc.).
    pub flags: u32,
    /// Version / epoch for optimistic validation (incremented on each overwrite).
    pub version: u64,
    /// Virtual / global offset or address within registered memory region (placeholder for now).
    pub address: u64,
    /// Remote key (rkey) for RDMA access (placeholder until real registration implemented).
    pub rkey: u32,
}

/// Internal metadata state stored in the directory map.
#[derive(Debug)]
struct DirectoryMeta {
    entry: DirectoryEntry,
}

/// Global (per-process) directory mapping key hashes to memory location metadata.
/// In a future phase, this will be sharded / replicated and synchronized across the cluster.
#[derive(Debug)]
pub struct Directory {
    entries: DashMap<u64, DirectoryMeta>,
    publish_version: AtomicU64,
}

impl Directory {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { entries: DashMap::new(), publish_version: AtomicU64::new(1) })
    }

    #[inline]
    pub fn hash_key<K: Hash + ?Sized>(key: &K) -> u64 {
        let mut hasher = AHasher::default();
        key.hash(&mut hasher);
        hasher.finish()
    }

    /// Publish (insert or update) a directory entry. Returns the new version.
    pub fn publish(&self, key_hash: u64, shard_id: u32, value_len: usize) -> u64 {
        let version = self.publish_version.fetch_add(1, Ordering::SeqCst) + 1;
        let entry = DirectoryEntry {
            key_hash,
            shard_id,
            value_len: value_len as u32,
            flags: 0,
            version,
            address: 0, // placeholder until real memory registration
            rkey: 0,
        };
        self.entries.insert(key_hash, DirectoryMeta { entry });
        version
    }

    /// Lookup by key hash.
    pub fn lookup_hash(&self, key_hash: u64) -> Option<DirectoryEntry> {
        self.entries.get(&key_hash).map(|m| m.entry)
    }

    /// Convenience lookup by full key string.
    pub fn lookup_key(&self, key: &str) -> Option<DirectoryEntry> {
        let h = Self::hash_key(key);
        self.lookup_hash(h)
    }
}
