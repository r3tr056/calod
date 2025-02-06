use chrono::{DateTime, Utc, Duration as ChronoDuration};
use dashmap::{DashMap, DashSet};
use serde_with::serde_as;
use core::f64;
use std::collections::LinkedList;
use serde::{Serialize, Deserialize};

// CacheEntry struct
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    #[serde(flatten)]
    pub value: DataType,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "chrono::serde::ts_milliseconds_option")]
    pub expires_at: Option<DateTime<Utc>>,
    size_bytes: usize,
}

impl CacheEntry {
    pub fn new(value: DataType, ttl: Option<ChronoDuration>) -> Self {
        let created_at = Utc::now();
        let expires_at = ttl.map(|duration| created_at + duration);
        let size_bytes = value.size();

        Self { value, expires_at, created_at, size_bytes }
    }

    pub fn size(&self) -> usize {
        self.size_bytes
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at.map_or(false, |exp| exp <= Utc::now())
    }

    pub fn eviction_score(&self) -> f64 {
        let age = Utc::now().timestamp_millis() - self.created_at.timestamp_millis();
        age as f64 * 0.7 + (self.expires_at.map_or(f64::MAX, |exp| (exp - Utc::now()).num_seconds() as f64) * 0.3)
    }

    pub fn expire_in(&mut self, duration: std::time::Duration) {
        self.expires_at = Some(Utc::now() + ChronoDuration::from_std(duration).unwrap_or(ChronoDuration::max_value()));
    }

    pub fn ttl(&self) -> Option<ChronoDuration> {
        self.expires_at.map(|expiry_time| {
            let now = Utc::now();
            expiry_time - now
        })
    }

    pub fn persist(&mut self) {
        self.expires_at = None;
    }
}

#[derive(Debug)]
pub struct EvictionCandidate {
    pub key: String,
    pub score: f64,
}

impl PartialEq for EvictionCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
    }
}

impl Eq for EvictionCandidate {}

impl PartialOrd for EvictionCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.score.partial_cmp(&other.score)
    }
}

impl Ord for EvictionCandidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.partial_cmp(other).unwrap_or(std::cmp::Ordering::Equal)
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum DataType {
    String(String),
    List(LinkedList<String>),
    Set(#[serde(with = "serde_dashset")] DashSet<String>),
    Hash(#[serde(with = "serde_dashmap")] DashMap<String, String>),
    Object {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        type_info: TypeInfo,
    },
}

impl DataType {
    fn size(&self) -> usize {
        match self {
            DataType::String(s) => s.len(),
            DataType::List(linked_list) => linked_list.len(),
            DataType::Set(dash_set) => dash_set.len(),
            DataType::Hash(dash_map) => dash_map.len(),
            DataType::Object { data, type_info: _ } => data.len(),
        }
    }
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

// DateTimeMeta serialization
#[derive(Serialize, Deserialize)]
pub struct DateTimeMeta {
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "chrono::serde::ts_milliseconds_option")]
    pub expire_at: Option<DateTime<Utc>>,
}

pub struct DateTimeMetaBuilder {
    created_at: DateTime<Utc>,
    expire_at: Option<DateTime<Utc>>,
}

// Implement conversion to/from DateTimeMetaBuilder
impl From<DateTimeMetaBuilder> for DateTimeMeta {
    fn from(builder: DateTimeMetaBuilder) -> Self {
        builder.build()
    }
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


// Custom serialization/deserialization for DashSet
mod serde_dashset {
    use super::*;
    use serde::{ser::SerializeSeq, Deserializer, Serializer};

    pub fn serialize<S>(set: &DashSet<String>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(set.len()))?;
        for item in set.iter() {
            seq.serialize_element(item.key())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DashSet<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let vec = Vec::<String>::deserialize(deserializer)?;
        Ok(DashSet::from_iter(vec))
    }
}

// Custom serialization/deserialization for DashMap
mod serde_dashmap {
    use super::*;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::collections::HashMap;

    pub fn serialize<S>(map: &DashMap<String, String>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut hm = HashMap::new();
        for entry in map.iter() {
            hm.insert(entry.key().clone(), entry.value().clone());
        }
        hm.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DashMap<String, String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let hm = HashMap::<String, String>::deserialize(deserializer)?;
        Ok(DashMap::from_iter(hm))
    }
}
