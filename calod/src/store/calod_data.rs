use core::f64;
use std::sync::Arc;
use atomic_refcell::AtomicRefCell;
use chrono::{DateTime, Utc, Duration as ChronoDuration};
use dashmap::{DashMap, DashSet};

use serde_with::serde_as;
use serde::{Serialize, Deserialize};
use serde_json::Value as JsonValue;

use crate::extensions::fastgraphdb::base::GraphData;

/// CacheEntry struct
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    #[serde(flatten)]
    pub value: DataType,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "chrono::serde::ts_milliseconds_option")]
    pub expires_at: Option<DateTime<Utc>>,
    // size_bytes: usize,
}

impl CacheEntry {
    pub fn new(value: DataType, ttl: Option<ChronoDuration>) -> Self {
        let created_at = Utc::now();
        let expires_at = ttl.map(|duration| created_at + duration);

        Self { value, expires_at, created_at}
    }

    pub fn size(&self) -> usize {
        self.value.size()
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at.map_or(false, |exp| exp <= Utc::now())
    }

    pub fn eviction_score(&self) -> f64 {
        let age_ms = Utc::now().timestamp_millis() - self.created_at.timestamp_millis().max(0); // // Ensure age is non-negative

        // Ensure expiry_seconds is non-negative
        let expiry_score = self.expires_at.map_or(f64::MAX, |exp| (exp - Utc::now()).num_seconds().max(0) as f64);
        
        let age_component = (age_ms * 7) >> 3; // age_ms * 0.875 (close to 0.7, using bit shift for division by 8)
        let expiry_component = (expiry_score * 3.0) / 10.0; // expiry_seconds * 0.3

        (age_component as f64) + expiry_component
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
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
    }
}

impl Eq for EvictionCandidate {}

impl PartialOrd for EvictionCandidate {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.score.partial_cmp(&other.score)
    }
}

impl Ord for EvictionCandidate {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.partial_cmp(other).unwrap_or(std::cmp::Ordering::Equal)
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum DataType {
    Nil,
    Integer(i64),
    String(String),
    List(Vec<String>),
    Set(#[serde(with = "serde_dashset")] DashSet<String>),
    Hash(#[serde(with = "serde_dashmap")] DashMap<String, String>),
    Document(JsonValue),
    Graph(GraphData),
    Object {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        type_info: TypeInfo,
    },
}

impl DataType {
    pub fn size(&self) -> usize {
        match self {
            DataType::Integer(_) => size_of::<i64>(),
            DataType::String(s) => s.len(),
            DataType::List(vec) => vec.iter().fold(0, |acc, s| acc + s.len()),
            DataType::Set(dash_set) => dash_set.len(),
            DataType::Hash(dash_map) => dash_map.len(),
            DataType::Document(json_value) => {
                if let Ok(serialized) = serde_json::to_string(json_value) {
                    serialized.len()
                } else {
                    0
                }
            },
            DataType::Object { data, type_info: _ } => data.len(),
            DataType::Graph(graph_data) => {
                let node_size: usize = graph_data.nodes.iter().map(|entry| entry.value().properties().len()).sum();
                let edge_size: usize = graph_data.edges.iter().map(|entry| entry.value().properties().len()).sum();
                node_size + edge_size + graph_data.nodes.len() + graph_data.edges.len()
            },
            DataType::Nil => 0
        }
    }

    pub fn data_type(&self) -> String {
        match self {
            DataType::Integer(_) => "Integer",
            DataType::String(_) => "String",
            DataType::List(_) => "List",
            DataType::Set(_) => "Set",
            DataType::Hash(_) => "DashMap",
            DataType::Document(_) => "Document",
            DataType::Graph(_) => "Graph",
            DataType::Object { .. } => "Object",
            DataType::Nil => "Nil",
        }.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeInfo {
    type_name: String,
    type_id: u32,
}

impl TypeInfo {
    #[inline(always)]
    pub fn is_valid(&self) -> bool {
        !self.type_name.is_empty() && self.type_id > 0
    }
}

#[derive(Debug, Clone)]
pub struct Set {
    data: Arc<AtomicRefCell<DashSet<String>>>,
}

impl Set {
    pub fn new() -> Self {
        Set {
            data: Arc::new(AtomicRefCell::new(DashSet::new())),
        }
    }

    pub fn insert(&self, value: String) {
        self.data.borrow_mut().insert(value);
    }

    pub fn contains(&self, value: &str) -> bool {
        self.data.borrow().contains(value)
    }

    pub fn remove(&self, value: &str) {
        self.data.borrow_mut().remove(value);
    }
}

#[derive(Debug, Clone)]
pub struct Hash {
    data: Arc<AtomicRefCell<DashMap<String, String>>>,
}

impl Hash {
    pub fn new() -> Self {
        Hash {
            data: Arc::new(AtomicRefCell::new(DashMap::new())),
        }
    }

    pub fn insert(&self, key: String, value: String) {
        self.data.borrow_mut().insert(key, value);
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.data.borrow().get(key).map(|entry| entry.value().clone())
    }

    pub fn remove(&self, key: &str) {
        self.data.borrow_mut().remove(key);
    }
}

/// DateTimeMeta serialization
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

/// Implement conversion to/from DateTimeMetaBuilder
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


/// Custom serialization/deserialization for DashSet
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

/// Custom serialization/deserialization for DashMap
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
