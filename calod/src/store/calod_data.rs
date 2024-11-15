use chrono::{DateTime, Utc};
use dashmap::{DashMap, DashSet};
use serde_derive::{Deserialize, Serialize};
use std::collections::LinkedList;

// CacheEntry struct
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub value: DataType,
    pub frequency: u32,
    pub last_accessed: DateTime<Utc>,
    pub ttl: Option<DateTime<Utc>>,
}

impl CacheEntry {
    pub fn is_expired(&self) -> bool {
        self.ttl.map(|expire| Utc::now() > expire).unwrap_or(false)
    }
}

pub struct CacheEntryWithScore {
    pub key: String,
    pub score: f64,
}

impl Ord for CacheEntryWithScore {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score.partial_cmp(&other.score).unwrap()
    }
}

impl Eq for CacheEntryWithScore {}

impl PartialOrd for CacheEntryWithScore {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for CacheEntryWithScore {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
    }
}

#[derive(Debug, Clone)]
pub enum DataType {
    String(String),
    List(LinkedList<String>),
    Set(Set),
    Hash(Hash),

    Object { data: Vec<u8>, type_info: TypeInfo },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeInfo {
    type_name: String,
    type_id: u64,
}

impl TypeInfo {
    pub fn is_valid(&self) -> bool {
        !self.type_name.is_empty() && self.type_id > 0
    }
}

#[derive(Debug, Clone)]
pub struct Set {
    data: DashSet<String>,
}

impl Set {
    pub fn new() -> Self {
        Set {
            data: DashSet::new(),
        }
    }

    pub fn insert(&self, value: String) {
        self.data.insert(value);
    }

    pub fn contains(&self, value: &str) -> bool {
        self.data.contains(value)
    }

    pub fn remove(&self, value: &str) {
        self.data.remove(value);
    }
}

#[derive(Debug, Clone)]
pub struct Hash {
    data: DashMap<String, String>,
}

impl Hash {
    pub fn new() -> Self {
        Hash {
            data: DashMap::new(),
        }
    }

    pub fn insert(&self, key: String, value: String) {
        self.data.insert(key, value);
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.data.get(key).map(|entry| entry.value().clone())
    }

    pub fn remove(&self, key: &str) {
        self.data.remove(key);
    }
}

#[derive(Debug)]
pub struct DateTimeMeta {
    pub created_at: DateTime<Utc>,
    pub expire_at: Option<DateTime<Utc>>,
}

pub struct DateTimeMetaBuilder {
    created_at: DateTime<Utc>,
    expire_at: Option<DateTime<Utc>>,
}

impl DateTimeMetaBuilder {
    pub fn new(created_at: DateTime<Utc>) -> Self {
        DateTimeMetaBuilder {
            created_at,
            expire_at: None,
        }
    }

    pub fn expire_at(mut self, expire_at: Option<DateTime<Utc>>) -> Self {
        self.expire_at = expire_at;
        self
    }

    pub fn build(self) -> DateTimeMeta {
        DateTimeMeta {
            created_at: self.created_at,
            expire_at: self.expire_at,
        }
    }
}
