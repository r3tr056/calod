
use once_cell::sync::Lazy;
use std::sync::RwLock;
use chrono::{Duration, Utc, Instant};
use std::collections::{VecDeque, BinaryHeap};
use dashmap::DashMap;
use std::sync::Once;
use std::sync::{Arc, Once, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use thiserror::Error;

use serial_test::serial;

use crate::store::calod_data::{CacheEntry, DateTimeMeta, DateTimeMetaBuilder};

static STORE: Lazy<RwLock<Option<CalodStore>>> = Lazy::new(|| RwLock::new(None));
static INIT: Once = Once::new();


#[derive(Debug)]
pub struct SetOptionalArgs {
	pub ttl: Duration
}

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
	start_time: Instant,
	request_count: u64,
	data: DashMap<String, CacheEntry>,
	lru_queue: Arc<Mutex<VecDeque<String>>>,
	capacity: AtomicUsize,
}


pub trait Store {
	fn initialize(capacity: usize);

	fn get_store() -> &'static mut CalodStore;

	fn get(&self, key: &str) -> Option<&str>;

	fn set(&mut self, key: &str, opt: &Option<SetOptionalArgs>) -> Option<DataType>;

	fn is_key_expired(&self, key: &str) -> bool;

	fn delete(&mut self, keys: Vec<&str>) -> u64;

	fn invalidate(&self);
}

impl Store for CalodStore {
	
	// Initialize the store globally using call_once (happens only once)
	// 1. Load the config from the env or json config file
	// 2. Acquire write lock on store
	// 3. Initialize the store with new structs
	fn initialize() {
		INIT.call_once(|| unsafe {
			let config = Config::from_env_or_file().expect("Failed to load config");

			let mut store = STORE.write().unwrap();
			*store = Some(CalodStore {
				data: DashMap::new(),
				lru_queue: Arc::new(Mutex::new(VecDeque::new())),
				capacity: AtomicUsize::new(config.cache_capacity),
			});
			println!("Store is initialized with capacity: {}", config.cache_capacity);
		});
	}


	// Retreive the store instance from the global variable
	// 1. Acquire read lock on the `STORE`
	// 2. If `STORE` is not None (Some), return a clone of Arc (Reference)
	// 3. Return an error is `STORE` is none
	fn get_store() -> Result<&'static mut CalodStore, CacheError> {
		let store = STORE.read().unwrap();
		if let Some(store) = store.as_ref() {
			Ok(Arc::clone(store))
		} else {
			Err(CacheError::StoreNotInitialized)
		}
	}


	// Retreive a value from the Calod Cache
	// 1. Check if the key exists in the `DashMap` -> Return KeyNotFound Error
	// 2. Check if the key has expired -> Return KeyExpired Error
	// 3. Get the `CacheEntry` from the cache
	// 4. Update the access meta `frequency` and `last_accessed`
	// 5. Move the key in the front of the LRU eviction queue
	// 6. Return the value store in cache entry
	fn get(&self, key: &str) -> Result<Option<&str>, CacheError> {
		if !self.data.contains_key(key) {
			return Err(CacheError::KeyNotFound(key.to_string()));
		}

		if self.is_key_expired(key?) {
			return Err(CacheError::KeyExpired(key.to_string()));
		}

		if let Some(mut entry) = self.data.get_mut(key) {
			entry.frequency += 1;
			entry.last_accessed = Utc::now();

			// Move the key to the front of the LRU queue
			let mut lru_queue = self.lru_queue.lock().unwrap();
			self.lru_queue.retain(|k| k != key);
			self.lru_queue.push_front(key.to_string());

			Ok(Some(entry.value.clone()))
		} else {
			Ok(None)
		}
	}

	// Insert/Update a value in the Calod cache
	// 1. Check if the cache capacity is full, if yes evict
	// 2. Calculate TTL of the `CacheEntry`
	// 3. Insert the cache entry into the `DashMap`
	// 4. Move the key to front of LRU eviction queue
	// 6. Return the old value of existed (in case update)
	fn set(&mut self, key: &str, value: &DataType, opt: &Option<SetOptionalArgs>) -> Option<DataType> {
		if self.data.len() >= self.capacity {
			self.evict();
		}

		let ttl_datetime = opt.map(|t| Utc::now() + t);
		let entry = CacheEntry {
			value: value.clone(),
			frequency: 1,
			last_accessed: Utc::now(),
			ttl: ttl_datetime,
		};

		// Insert and handle the previous entry properly
		let old_entry = self.data.insert(key.to_string(), entry);

		// Move the key to the front of the LRU queue
		self.lru_queue.push_front(key.to_string());

		Ok(old_entry.map(|e| e.value))
	}

	// Evict and entry from the cache LRU/LFU/Predictive Weights
	// 1. Iterate through the LRU queue to calculate priority scores
	// 2. Calcaulate Weights LRU + LFU + Predictive
	// 3. Push the entry onto the min heap
	// 4. Pop the lowest priority entry from the heap and evict it
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

	// Calculate the LRU weight
	// 1. Calculate the time since last accessed
	fn calculate_lru_weight(&self, entry: &CacheEntry) -> f64 {
		let now = Utc::now();
		let duration_since_last_access = now - entry.last_accessed;
		duration_since_last_access.num_milliseconds() as f64;
	}

	// Calculate the LFU weight using inverse of frequency
	// Add 1.0 to avoid Divide by Zero
	fn calculate_lfu_weight(&self, entry: &CacheEntry) -> f64 {
		1.0 / (entry.frequency as f64 + 1.0)
	}

	// Calculate the Predictive weight based on TTL
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

	// Check if key is expired based on ttl
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

	// Delete Cache Entries from CalodStore and LRU -> Iterate and delete
	fn delete(&mut self, keys: Vec<&str>) -> Result<u64, CacheError> {
		let mut delete_count = 0;
		
		for key in keys {
			if self.data.remove(key).is_some() {
				self.lru_queue.retain(|k| k != key);
				delete_count += 1;
			}
		}

		Ok(delete_count)
	}

	// Invalidate keys form the cache
	// 1. Iterate through the cache entries, checking the TTL and add them to the vec
	// 2. Delete them for fuck sake!
	fn invalidate(&self) {
		let now = Utc::now();
		let mut keys_to_remove = Vec::new();

		for entry in self.data.iter() {
			if let Some(expire_at) = entry.ttl {
				if expire_at < now {
					keys_to_remove.push(entry.key().clone());
				}
			}
		}

		for key in keys_to_remove {
			self.data.remove(&key);
			println!("Invalidation: Key {} has been removed.", key);
		}
	}

	pub fn increment_request_count(&mut self) {
		self.request_count += 1;
	}

	pub fn get_stats(&self) -> String {
		let uptime = self.start_time.elapsed();
		let response_time_avg = self.calculate_avg_response_time();

		format!("Uptime: {:?}\nRequests Handled: {}\nAverage Response Time: {:?}", uptime, self.request_count, response_time_avg)
	}
}

impl CalodStore {
	pub fn reset() {
		unsafe {
			let mut store = STORE.write().unwrap();
			if let Some(mut s) = store.take() {
				s.data.clear();
				println!("Store is reset.");
			} else {
				println!("Store is already reset.");
			}
		}
	}

	pub fn load_from_file(file_path: &str) -> Result<Self> {
		let file_content = fs::read_to_string(file_path)?;
		let store: CalodStore = serde_json::from_str(&file_content)?;
		Ok(store)
	}

	pub fn save_to_file(&self, file_path: &str) -> Result<()> {
		let json_data = serde_json::to_string(self)?;
		fs::write(file_path, json_data)?;
		Ok(())
	}
}