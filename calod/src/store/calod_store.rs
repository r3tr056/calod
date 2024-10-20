
use once_cell::sync::Lazy;
use std::sync::RwLock;
use chrono::{Duration, Utc};
use std::collections::{VecDeque};
use dashmap::DashMap;
use std::sync::Once;
use thiserror::Error;


use serial_test::serial;

use crate::store::calod_data::{CacheEntry, DateTimeMeta, DateTimeMetaBuilder};
use crate::store::calod_ops::SetOptionalArgs;

static STORE: Lazy<RwLock<Option<CalodStore>>> = Lazy::new(|| RwLock::new(None));
static INIT: Once = Once::new();
static mut INIT_COUNT: u8 = 0;

#[derive(Debug, Error)]
pub enum CacheError {
	#[error("Key `{0}` not found")]
	KeyNotFound(String),

	#[error("Store is not initialized")]
	StoreNotInitialized,

	#[error("Key `{0}` has expired")]
	KeyExpired(String),

	#[error("Invalid TTL value provided")]
	InvalidTtl,
}


#[derive(Debug)]
pub struct CalodStore {
	data: DashMap<String, CacheEntry>,
	lru_queue: VecDeque<String>,
	capacity: usize,
}

struct CacheEntryWithScore {
	key: String,
	score: f64,
}

pub trait Store {
	fn initialize();

	fn get_store() -> &'static mut CalodStore;

	fn get(&self, key: &str) -> Option<&str>;

	// Returns None if key is not present previously, or the old value of the key
	fn set(&mut self, key: &str, opt: &Option<SetOptionalArgs>) -> Option<DataType>;

	fn is_key_expired(&self, key: &str) -> bool;

	// Return number of key that are deleted
	fn delete(&mut self, keys: Vec<&str>) -> u64;

	fn invalidate(&self);
}

impl Store for CalodStore {
	#[cfg(not(feature="init_calod_test"))]
	fn initialize() {
		INIT.call_once(|| unsafe {
			STORE = Some(CalodStore {
				data: DashMap::new(),
				lru_queue: VecDeque::new(),
				datetime: DashMap::new(),
			});
			INIT_COUNT += 1;
			println!("Store is initialized.");
		});
	}

	#[cfg(feature="init_calod_test")]
	fn initialize() {
		unsafe {
			STORE = Some(CalodStore {
				data: HashMap::new(),
				datetime: HashMap::new(),
			});
			INIT_COUNT += 1;
		}
		println!("Store is initialized in test mode.");
	}

	fn get_store() -< Result<&'static mut CalodStore, CacheError> {
		unsafe {
			if STORE.is_none() {
				return Err(CacheError::StoreNotInitialized);
			}
			Ok(STORE.as_mut().unwrap());
		}
	}

	fn get(&self, key: &str) -> Result<Option<&str>, CacheError> {

		if !self.data.contains_key(key) {
			return Err(CacheError::KeyNotFound(key.to_string()));
		}

		if self.is_key_expired(key?) {
			return Err(CacheError::KeyExpired(key.to_string()));
		}

		if let Some(entry) = self.data.get_mut(key) {
			entry.frequency += 1;
			entry.last_accessed = Utc::now();

			// move the key to the front of the LRU queue
			self.lru_queue.retain(|k| k != key);
			self.lru_queue.push_front(key.to_string());

			Ok(Some(entry.value.clone()))
		} else {
			Ok(None)
		}
	}

	fn set(&mut self, key: &str, value: &str, opt: &Option<SetOptionalArgs>) -> Option<DataType> {
		if self.data.len() >= self.capacity {
			self.evict();
		}

		let ttl_datetime = opt.ttl.map(|t| Utc::now() + t);
		let entry = CacheEntry {
			value: value.to_string(),
			frequency: 1,
			last_accessed: Utc::now(),
			ttl: ttl_datetime,
		};

		// Insert and handle the previous entry properly
		let old_entry = self.data.insert(key.to_string(), entry);

		// Move the key to the front of the LRU queue
		self.lru_queue.push_front(key.to_string());

		Ok(old_entry)
	}

	fn evict(&mut self) {
		let mut heap = BinaryHeap::new();

		for key in self.lru_queue.iter() {
			if let Some(entry) = self.data.get(key) {
				let lru_weight = self.calculate_lru_weight(entry);
				let lfu_weight = self.calculate_lfu_weight(entry);
				let predictive_weight = self.calculate_predictive_weight(entry);

				// Combine the weights into a single priority score
				let total_priority_score = lru_weight + lfu_weight + predictive_weight;

				heap.push(CacheEntryWithScore { key: key.clone(), score: total_priority_score });
			}
		}

		if let Some(lowest) = heap.pop() {
			self.data.remove(&lowest.key);
			self.lru_queue.retain(|k| k != &lowest.key);
		}
	}

	fn calculate_lru_weight(&self, entry: &CacheEntry) -> f64 {
		let now = Utc::now();
		let duration_since_last_access = now - entry.last_accessed;
		duration_since_last_access.num_milliseconds() as f64;
	}

	fn calculate_lfu_weight(&self, entry: &CacheEntry) -> f64 {
		1.0 / (entry.frequency as f64 + 1.0)
	}

	fn calculate_predictive_weight(&self, entry: &CacheEntry) -> f64 {
		if let Some(ttl) = entry.ttl {
			let now = Utc::now();
			let time_until_expiry = ttl - now;

			if time_until_expiry.num_seconds() <= 0 {
				return f64::MAX;
			} else {
				return 1.0 / (time_until_expiry.num_milliseconds() as f64);
			}
		}
		// if there's no TTL, we assume it's not expiring soon
		0.0
	}  

	fn is_key_expired(&self, key: &str) -> Result<bool, CacheError> {
		let now = Utc::now();

		if let Some(entry) = self.data.get(key) {
			if let Some(ttl) = entry.ttl {
				return Ok(ttl < now);
			}
			return Ok(false);
		}

		Err(CacheError::KeyNotFound(key.to_string()))
	}

	fn delete(&mut self, keys: Vec<&str>) -> Result<u64, CacheError> {
		let mut delete_count = 0;
		
		for key in keys {
			let key_str = key.to_string();
			let removed_data = self.data.remove(key_str.as_str());
			
		}
	}

	fn invalidate(&self) {
		let now = Utc::now();
		let mut keys_to_remove = Vec::new();

		for entry in self.datetime.iter() {
			if let Some(expire_at) = entry.value().expire_at {
				if expire_at < now {
					keys_to_remove.push(entry.key().clone());
				}
			}
		}

		for key in keys_to_remove {
			self.data.remove(&key);
			self.datetime.remove(&key);
			println!("Invalidation: Key {} has been removed.", key);
		}
	}
}

impl CalodStore {
	pub fn reset() {
		unsafe {
			if let Some(mut store) = STORE.take() {
				store.data.clear();
				INIT_COUNT = 0;
				println!("Store is reset.");
			} else {
				println!("Store is already reset.");
			}
		}
	}
}