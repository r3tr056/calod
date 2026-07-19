use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tracing::info;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Credential {
    password_hash: String,
}

#[derive(Clone, Debug)]
struct AuthCacheEntry {
    expires_at: Instant,
}

pub struct AuthManager {
    auth_required: bool,
    credentials: RwLock<HashMap<String, Credential>>,
    auth_cache: DashMap<String, AuthCacheEntry, ahash::RandomState>,
    cache_ttl: Duration,
    argon2: Argon2<'static>,
}

impl AuthManager {
    pub fn new(auth_required: bool) -> Self {
        Self {
            auth_required,
            credentials: RwLock::new(HashMap::new()),
            auth_cache: DashMap::with_hasher(ahash::RandomState::new()),
            cache_ttl: Duration::from_secs(300),
            argon2: Argon2::default(),
        }
    }

    #[inline]
    pub fn auth_required(&self) -> bool {
        self.auth_required
    }

    pub fn add_password(&self, password: &str) {
        if !self.auth_required {
            return;
        }

        let salt = SaltString::generate(&mut OsRng);
        let password_hash = match self.argon2.hash_password(password.as_bytes(), &salt) {
            Ok(hash) => hash.to_string(),
            Err(err) => {
                info!("failed to hash password: {}", err);
                return;
            }
        };

        let credential = Credential { password_hash };

        if let Ok(mut guard) = self.credentials.write() {
            guard.insert("default".to_string(), credential);
            info!("Added password for default user");
        }
    }

    pub fn authenticate(&self, password: &str) -> bool {
        if !self.auth_required {
            return true;
        }

        let cache_key = self.cache_key(password);
        if let Some(entry) = self.auth_cache.get(&cache_key) {
            if Instant::now() <= entry.expires_at {
                return true;
            }
            self.auth_cache.remove(&cache_key);
        }

        let Ok(credentials) = self.credentials.read() else {
            return false;
        };

        for cred in credentials.values() {
            let Ok(parsed_hash) = PasswordHash::new(&cred.password_hash) else {
                continue;
            };

            if self
                .argon2
                .verify_password(password.as_bytes(), &parsed_hash)
                .is_ok()
            {
                self.auth_cache.insert(
                    cache_key,
                    AuthCacheEntry {
                        expires_at: Instant::now() + self.cache_ttl,
                    },
                );
                return true;
            }
        }

        false
    }

    fn cache_key(&self, password: &str) -> String {
        use std::hash::Hasher;
        let mut hasher = twox_hash::XxHash64::default();
        hasher.write(password.as_bytes());
        format!("{:x}", hasher.finish())
    }

    pub fn rotate_credentials(&self) {
        if !self.auth_required {
            return;
        }
        let now = Instant::now();
        self.auth_cache.retain(|_, entry| now <= entry.expires_at);
    }
}

impl Default for AuthManager {
    fn default() -> Self {
        Self::new(false)
    }
}

pub fn create_auth_manager(password: Option<&str>) -> Arc<AuthManager> {
    let auth_required = password.is_some();
    let auth_manager = Arc::new(AuthManager::new(auth_required));

    if let Some(pass) = password {
        auth_manager.add_password(pass);
    }

    auth_manager
}
