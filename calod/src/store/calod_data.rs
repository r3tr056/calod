use dashmap::DashMap;
use chrono::{DateTime, Utc};

// CacheEntry struct
#[derive(Debug, Clone)]
pub struct CacheEntry {
	value: DataType,
	frequency: u32,
	last_accessed: DateTime<Utc>,
	ttl: Option<DateTime<Utc>>,
}

struct CacheEntryWithScore {
	key: String,
	score: f64,
}

impl Ord for CacheEntryWithScore {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.score.partial_cmp(&other.score).unwrap()
	}
}

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

impl Eq for CacheEntryWithScore {}

#[derive(Debug)]
pub enum DataType {
	String(String),
	List(LinkedList<String>),
	Set(Set),
	Hash(Hash),
}

#[derive(Debug)]
pub struct LinkedList {
	head: LinkedListNode,
}

#[derive(Debug)]
pub struct LinkedListNode {
	data: String,
	next: Option<Box<LinkedList>>,
}

#[derive(Debug)]
pub struct Set {
	data: DashMap<String, ()>,
}

impl Set {
	pub fn new() -> Self {
		Set { data: DashMap::new(), }
	}

	pub fn insert(&self, value: String) {
		self.data.insert(value, ());
	}

	pub fn contains(&self, value: &str) -> bool {
		self.data.contains_key(value)
	}

	pub fn remove(&self, value: &str) {
		self.data.remove(value);
	}
}

#[derive(Debug)]
pub struct Hash {
	data: DashMap<String, String>,
}

impl Hash {
	pub fn new() -> Self {
		Hash { data: DashMap::new(), }
	}

	pub fn insert(&self, key: String, value: String) {
		self.data.insert(key, value);
	}

	pub fn get(&self, key: &str) -> Option<String> {
		self.data.get(key).map(|entry| entry.clone())
	}

	pub fn remove(&self, key: &str) {
		self.data.remove(key);
	}
}

#[derive(Debug)]
pub struct DateTimeMeta {
	pub created_at: DateTime<Utc>,
	pub expire_at: Option<DateTime<Utc>>
}

pub struct DateTimeMetaBuilder {
	created_at: DateTime<Utc>,
	expire_at: Option<DateTime<Utc>>
}

impl DateTimeMetaBuilder {
	pub fn new(created_at: DateTime<Utc>) -> DateTimeMetaBuilder {
		DateTimeMetaBuilder {
			created_at,
			expire_at: None
		}
	}

	pub fn expire_at(mut self, expire_at: Option<DateTime<Utc>>) -> DateTimeMetaBuilder {
		self.expire_at = expire_at;
		self
	}

	pub fn build(self) -> DateTimeMeta {
		DateTimeMeta {
			created_at: self.created_at,
			expire_at: self.expire_at
		}
	}
}