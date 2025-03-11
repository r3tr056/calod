use std::collections::HashSet;
use std::hash::Hash;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use blake3::Hasher;
use dashmap::DashMap;
use rand::{Rng, rng};
use rand::distr::Alphanumeric;
use base64::{Engine as _, engine::general_purpose};
use tracing::info;
use once_cell::sync::Lazy;


#[derive(Debug, PartialEq, Eq)]
pub enum AuthResult {
    Success,
    Failed,
    Denied,
}

#[derive(Clone, Debug)]
struct Credential {
    user_id: String,
    password_hash: String,
    salt: String,
    created_at: Instant,
    last_used: Instant,
    permissions: Vec<String>,
}

impl PartialEq for Credential {
    fn eq(&self, other: &Self) -> bool {
        self.user_id == other.user_id
    }
}

impl Eq for Credential {}

impl Hash for Credential {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.user_id.hash(state);
    }
}

/// Cache entry for fast authentication checking
#[derive(Clone, Debug)]
struct AuthCacheEntry {
    password_hash: u64,
    timestamp: Instant,
}

/// Authentication manager with security features
pub struct AuthManager {
    // configuration
    auth_required: bool,
    // Credential storage (protected for infrequent writes) 
    credentials: RwLock<HashSet<Credential>>,
    // fast authentication cache for repeated logins (lock-free)
    auth_cache: DashMap<String, AuthCacheEntry, ahash::RandomState>,
    // cache settings
    cache_ttl: Duration,
    // password hashing parameters
    hash_iterations: u32
}

// Global default salt for when no specific salt is available
static DEFAULT_SALT: Lazy<String> = Lazy::new(|| {
    rng()
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(char::from)
        .collect()
});

impl AuthManager {
    pub fn new(auth_required: bool) -> Self {
        Self {
            auth_required,
            credentials: RwLock::new(HashSet::new()),
            auth_cache: DashMap::with_hasher(ahash::RandomState::new()),
            cache_ttl: Duration::from_secs(3600),
            hash_iterations: if auth_required { 3 } else { 1 },
        }
    }

    #[inline]
    pub fn auth_required(&self) -> bool {
        self.auth_required
    }

    /// Add a password
    pub fn add_password(&self, password: &str) {
        if !self.auth_required { return; }

        // get random salt
        let salt: String = rng()
            .sample_iter(&Alphanumeric)
            .take(16)
            .map(char::from)
            .collect();

        // hash the password with the salt
        let password_hash = self.hash_password_secure(password, &salt);

        // create a new credential
        let credential = Credential {
            user_id: "default".to_string(),
            password_hash,
            salt,
            created_at: Instant::now(),
            last_used: Instant::now(),
            permissions: vec!["*".to_string()],
        };

        // store the credential
        let mut credentials = self.credentials.write().unwrap();
        credentials.insert(credential);
        
        
        info!("Added password for default user");
    }

    /// Add a user with password
    pub fn add_user(&self, username: &str, password: &str, permissions: Vec<String>) -> bool {
        if !self.auth_required {
            return false;
        }
        
        // Generate a random salt
        let salt: String = rng()
            .sample_iter(&Alphanumeric)
            .take(16)
            .map(char::from)
            .collect();
            
        // Hash the password with the salt
        let password_hash = self.hash_password_secure(password, &salt);
        
        // Create a new credential
        let credential = Credential {
            user_id: username.to_string(),
            password_hash,
            salt,
            created_at: Instant::now(),
            last_used: Instant::now(),
            permissions,
        };
        
        // Store the credential
        let mut credentials = self.credentials.write().unwrap();
        credentials.insert(credential);
        
        info!("Added password for user {}", username);
        true
    }
    
    /// Remove a user
    pub fn remove_user(&self, username: &str) -> bool {
        if !self.auth_required {
            return false;
        }
        
        let mut credentials = self.credentials.write().unwrap();
        let before_len = credentials.len();
        
        credentials.retain(|cred| cred.user_id != username);
        
        // Also remove from cache
        self.auth_cache.remove(username);
        
        before_len != credentials.len()
    }
    
    /// Authenticate a password
    #[inline]
    pub fn authenticate(&self, password: &str) -> bool {
        if !self.auth_required {
            return true;
        }
        
        // Fast path: check cache first
        let cache_key = self.fast_hash_for_cache(password);
        if let Some(cache_entry) = self.auth_cache.get(&cache_key) {
            if cache_entry.timestamp.elapsed() < self.cache_ttl {
                // Still valid cache entry
                return true;
            }
            // Cache entry expired, remove it
            self.auth_cache.remove(&cache_key);
        }
        
        // Slow path: need to perform full authentication
        let credentials = self.credentials.read().unwrap();
        
        // If we have any credential, we can compare against it
        // (This implementation uses a default user with all permissions)
        for cred in credentials.iter() {
            let hashed = self.hash_password_secure(password, &cred.salt);
            if hashed == cred.password_hash {
                // Add to cache for fast future lookups
                self.auth_cache.insert(
                    cache_key,
                    AuthCacheEntry {
                        password_hash: self.xxhash_password(password),
                        timestamp: Instant::now(),
                    }
                );
                return true;
            }
        }
        
        // Authentication failed
        false
    }
    
    /// Generate a fast cache key using xxHash
    #[inline]
    fn fast_hash_for_cache(&self, password: &str) -> String {
        // Using a simple hash for the cache key - not for security, just for lookups
        format!("{:x}", self.xxhash_password(password))
    }
    
    /// Hash a password using xxHash for maximum speed (internal use only)
    #[inline]
    fn xxhash_password(&self, password: &str) -> u64 {
        use std::hash::Hasher;
        let mut hasher = twox_hash::XxHash64::default();
        hasher.write(password.as_bytes());
        hasher.finish()
    }
    
    /// Hash a password using a secure algorithm (BLAKE3)
    fn hash_password_secure(&self, password: &str, salt: &str) -> String {
        // Start with the base hash
        let mut hasher = Hasher::new();
        hasher.update(password.as_bytes());
        hasher.update(salt.as_bytes());
        
        let mut hash = hasher.finalize();
        
        // Multiple iterations for added security
        for _ in 1..self.hash_iterations {
            let mut hasher = Hasher::new();
            hasher.update(hash.as_bytes());
            hasher.update(salt.as_bytes());
            hash = hasher.finalize();
        }
        
        general_purpose::STANDARD_NO_PAD.encode(hash.as_bytes())
    }
    
    /// Perform password rotation and security maintenance
    pub fn rotate_credentials(&self) {
        if !self.auth_required {
            return;
        }
        
        // Clean expired cache entries
        self.auth_cache.retain(|_, entry| {
            entry.timestamp.elapsed() < self.cache_ttl
        });
    }
}


impl Default for AuthManager {
    fn default() -> Self {
        // By default, don't require authentication
        Self::new(false)
    }
}

/// Create a shared authentication manager
pub fn create_auth_manager(password: Option<&str>) -> Arc<AuthManager> {
    let auth_required = password.is_some();
    let auth_manager = Arc::new(AuthManager::new(auth_required));
    
    if let Some(pass) = password {
        auth_manager.add_password(pass);
    }
    
    auth_manager
}