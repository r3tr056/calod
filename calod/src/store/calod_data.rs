use dashmap::{DashMap, DashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use serde_with::serde_as;

use crate::store::graph::graph_data::GraphData;
use crate::store::sorted_set::SortedSetData;
use crate::store::stream::StreamData;

fn size_of<T>() -> usize {
    std::mem::size_of::<T>()
}

/// CacheEntry struct
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    pub value: DataType,
    /// Created at Timestamp in millis
    pub created_at_ms: u64,
    /// Expiry timestamp in millis
    pub expires_at_ms: u64,
}

impl CacheEntry {
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    pub fn new(value: DataType, ttl: Option<Duration>) -> Self {
        let now_ms = Self::now_ms();

        let expires_at_ms = ttl
            .map(|duration| now_ms + duration.as_millis() as u64)
            .unwrap_or(0);

        Self {
            value,
            created_at_ms: now_ms,
            expires_at_ms,
        }
    }

    pub fn size(&self) -> usize {
        self.value.size()
    }

    pub fn is_expired(&self) -> bool {
        if self.expires_at_ms == 0 {
            false
        } else {
            let now_ms = Self::now_ms();
            now_ms >= self.expires_at_ms
        }
    }

    pub fn eviction_score(&self) -> f64 {
        let now_ms = Self::now_ms();
        let age_ms = now_ms.saturating_sub(self.created_at_ms);

        let expiry_score = if self.expires_at_ms == 0 {
            f64::MAX
        } else {
            self.expires_at_ms.saturating_sub(now_ms) as f64 / 1000.0
        };

        let age_component = (age_ms * 7) >> 3; // age_ms * 0.875 (close to 0.7, using bit shift for division by 8)
        let expiry_component = (expiry_score * 3.0) / 10.0; // expiry_seconds * 0.3

        (age_component as f64) + expiry_component
    }

    pub fn expire_in(&mut self, duration: std::time::Duration) {
        let now_ms = Self::now_ms();
        self.expires_at_ms = now_ms + duration.as_millis() as u64;
    }

    pub fn expire_at_ms(&mut self, when_ms: u64) {
        self.expires_at_ms = when_ms;
    }

    pub fn ttl(&self) -> Option<i64> {
        if self.expires_at_ms == 0 {
            None
        } else {
            let now_ms = Self::now_ms();
            Some((self.expires_at_ms.saturating_sub(now_ms) / 1000) as i64)
        }
    }

    pub fn pttl(&self) -> Option<i64> {
        if self.expires_at_ms == 0 {
            None
        } else {
            let now_ms = Self::now_ms();
            Some(self.expires_at_ms.saturating_sub(now_ms) as i64)
        }
    }

    pub fn expiretime(&self) -> Option<i64> {
        if self.expires_at_ms == 0 {
            None
        } else {
            Some((self.expires_at_ms / 1000) as i64)
        }
    }

    pub fn pexpiretime(&self) -> Option<i64> {
        if self.expires_at_ms == 0 {
            None
        } else {
            Some(self.expires_at_ms as i64)
        }
    }

    pub fn persist(&mut self) {
        self.expires_at_ms = 0;
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
        Some(self.cmp(other))
    }
}

impl Ord for EvictionCandidate {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score.total_cmp(&other.score)
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DataType {
    Nil,
    /// A null *aggregate* reply. Distinct from [`DataType::Nil`] (null bulk
    /// string, `$-1` in RESP2) so commands whose non-null result is an array —
    /// `BLPOP`/`BRPOP` on timeout, `EXEC` on abort — emit the correct `*-1`
    /// (RESP2) / `_` (RESP3) framing. Transient reply marker only; never stored.
    NilArray,
    Integer(i64),
    Status(String),
    /// A binary-safe string value. Stored as raw bytes so that arbitrary binary
    /// payloads (protobuf, msgpack, compressed blobs, raw integers) round-trip
    /// without corruption, matching Redis semantics. (C1: binary safety.)
    String(#[serde(with = "serde_bytes")] Vec<u8>),
    Array(Vec<DataType>),
    /// List elements are binary-safe (raw bytes), matching Redis. (C1)
    List(Vec<Vec<u8>>),
    /// Set members are binary-safe (raw bytes), matching Redis. (C1)
    Set(#[serde(with = "serde_dashset")] DashSet<Vec<u8>>),
    /// Hash fields and values are binary-safe (raw bytes), matching Redis. (C1)
    Hash(#[serde(with = "serde_dashmap")] DashMap<Vec<u8>, Vec<u8>>),
    /// Sorted set: binary-safe members with f64 scores, ordered by (score,
    /// member). (Phase 5)
    SortedSet(SortedSetData),
    /// Stream: an append-only log of entries keyed by [`StreamId`], each entry a
    /// list of binary-safe field/value pairs. (Phase 8)
    Stream(StreamData),
    /// A floating-point reply value (e.g. ZSCORE/ZINCRBY). RESP2 renders it as a
    /// bulk string, RESP3 as a `,` double. Transient reply marker — never stored
    /// as a key's value.
    Double(f64),
    Document(JsonValue),
    Graph(GraphData),
    Object {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        type_info: TypeInfo,
    },
}

impl DataType {
    /// Construct a binary-safe `String` value from any byte source
    /// (`String`, `&str`, `Vec<u8>`, `&[u8]`). Lossless for all inputs.
    #[inline]
    pub fn string_from(bytes: impl Into<Vec<u8>>) -> DataType {
        DataType::String(bytes.into())
    }

    /// If this is a `String`/`Status`, return its bytes; otherwise `None`.
    #[inline]
    pub fn as_str_bytes(&self) -> Option<&[u8]> {
        match self {
            DataType::String(b) => Some(b.as_slice()),
            DataType::Status(s) => Some(s.as_bytes()),
            _ => None,
        }
    }

    pub fn size(&self) -> usize {
        match self {
            DataType::Integer(_) => size_of::<i64>(),
            DataType::Status(s) => s.len(),
            DataType::String(s) => s.len(),
            DataType::Array(values) => values.iter().map(DataType::size).sum(),
            DataType::List(vec) => vec.iter().fold(0, |acc, s| acc + s.len()),
            DataType::Set(dash_set) => dash_set.len(),
            DataType::Hash(dash_map) => dash_map.len(),
            DataType::Document(json_value) => {
                if let Ok(serialized) = serde_json::to_string(json_value) {
                    serialized.len()
                } else {
                    0
                }
            }
            DataType::Object { data, type_info: _ } => data.len(),
            DataType::Graph(graph_data) => {
                let node_size: usize = graph_data
                    .nodes
                    .iter()
                    .map(|entry| entry.value().properties().len())
                    .sum();
                let edge_size: usize = graph_data
                    .edges
                    .iter()
                    .map(|entry| entry.value().properties().len())
                    .sum();
                node_size + edge_size + graph_data.nodes.len() + graph_data.edges.len()
            }
            DataType::SortedSet(ss) => ss.len() * (size_of::<f64>() + 16),
            DataType::Stream(st) => st.len() * 32,
            DataType::Double(_) => size_of::<f64>(),
            DataType::Nil => 0,
            DataType::NilArray => 0,
        }
    }

    pub fn data_type(&self) -> String {
        match self {
            DataType::Integer(_) => "Integer",
            DataType::Status(_) => "Status",
            DataType::String(_) => "String",
            DataType::Array(_) => "Array",
            DataType::List(_) => "List",
            DataType::Set(_) => "Set",
            DataType::Hash(_) => "DashMap",
            DataType::Document(_) => "Document",
            DataType::Graph(_) => "Graph",
            DataType::Object { .. } => "Object",
            DataType::SortedSet(_) => "SortedSet",
            DataType::Stream(_) => "Stream",
            DataType::Double(_) => "Double",
            DataType::Nil => "Nil",
            DataType::NilArray => "Nil",
        }
        .to_string()
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

/// Custom serialization/deserialization for DashSet
mod serde_dashset {
    use super::*;
    use serde::{ser::SerializeSeq, Deserializer, Serializer};

    pub fn serialize<S>(set: &DashSet<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(set.len()))?;
        for item in set.iter() {
            seq.serialize_element(item.key())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DashSet<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let vec = Vec::<Vec<u8>>::deserialize(deserializer)?;
        Ok(DashSet::from_iter(vec))
    }
}

/// Custom serialization/deserialization for DashMap
mod serde_dashmap {
    use super::*;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(map: &DashMap<Vec<u8>, Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Serialize as a sequence of byte pairs rather than a native map, so the
        // binary keys round-trip through serializers that require string map keys
        // (e.g. JSON) as well as bincode.
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = map
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        pairs.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DashMap<Vec<u8>, Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let pairs = Vec::<(Vec<u8>, Vec<u8>)>::deserialize(deserializer)?;
        Ok(DashMap::from_iter(pairs))
    }
}
