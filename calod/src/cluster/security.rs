use std::{collections::HashMap, io::Error, sync::Arc, time::{Duration, Instant}};

use aes_gcm::Aes256Gcm;
use async_std::sync::RwLock;
use base64::{engine::general_purpose, Engine};
use rand::{rng, RngCore};
use uuid::Uuid;

use super::config::RdmaConfig;

/// Security manager for RDMA operations
pub struct SecurityManager {
    /// RDMA config
    config: Arc<RdmaConfig>,

    /// Authentication tokens
    auth_tokens: RwLock<HashMap<String, AuthToken>>,

    /// Access control manager
    access_control: AccessControlManager,

    /// key manager
    key_manager: KeyManager,
}

/// Authentication token
pub struct AuthToken {
    /// Token ID
    id: Uuid,

    /// Node ID
    node_id: String,

    /// Token value
    token: String,

    /// Expiration time
    expires_at: Instant,
}

/// Encryption context for secure communication
pub struct EncryptionContext {
    /// Encryption key
    key: [u8; 32],
    
    /// Node ID this context is for
    node_id: String,
    
    /// Key ID
    key_id: Uuid,
    
    /// Cipher instance
    cipher: Aes256Gcm,
}

/// Access control manager
struct AccessControlManager {
    /// Node access policies
    node_policies: RwLock<HashMap<String, AccessPolicy>>,
    
    /// Default policy
    default_policy: AccessPolicy,
}

/// Access policy for a node
#[derive(Clone)]
struct AccessPolicy {
    /// Can read from local node
    can_read: bool,
    
    /// Can write to local node
    can_write: bool,
    
    /// Can perform atomic operations
    can_atomic: bool,
    
    /// Permitted memory regions (empty = all)
    permitted_regions: Vec<Uuid>,
}

/// Key manager for encryption keys
struct KeyManager {
    /// Master key
    master_key: [u8; 32],
    
    /// Node keys
    node_keys: RwLock<HashMap<String, NodeKey>>,
    
    /// Key rotation interval
    rotation_interval: Duration,
    
    /// Last rotation time
    last_rotation: RwLock<Instant>,
}

/// Node encryption key
struct NodeKey {
    /// Key ID
    id: Uuid,
    
    /// Encryption key
    key: [u8; 32],
    
    /// HMAC key
    hmac_key: [u8; 32],
    
    /// Created at
    created_at: Instant,
    
    /// Expires at
    expires_at: Instant,
}

impl SecurityManager {
    pub fn new(config: Arc<RdmaConfig>) -> Result<Self, Error> {
        let mut master_key = [0u8; 32];
        rng().fill_bytes(&mut master_key);

        let key_manager = KeyManager {
            master_key,
            node_keys: RwLock::new(HashMap::new()),
            rotation_interval: Duration::from_secs(config.key_rotation_interval_secs),
            last_rotation: RwLock::new(Instant::now()),
        };

        let access_control = AccessControlManager {
            node_policies: RwLock::new(HashMap::new()),
            default_policy: AccessPolicy {
                can_read: config.default_allow_remote_read,
                can_write: config.default_allow_remote_write,
                can_atomic: config.default_allow_remote_atomic,
                permitted_regions: Vec::new(),
            },
        };

        Ok(Self {
            config,
            auth_tokens: RwLock::new(HashMap::new()),
            access_control,
            key_manager
        })
    }

    pub async fn generate_auth_token(&self) -> Result<String, Error> {
        const TOKEN_LENGTH: usize = 32;
        let mut token_bytes = vec![0u8; TOKEN_LENGTH];
        rng().fill_bytes(&mut token_bytes);

        let token = general_purpose::STANDARD.encode(&token_bytes);

        let auth_token = AuthToken {
            id: Uuid::new_v4(),
            node_id: "local".to_string(),
            token: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(self.config.auth_token_expiry_secs),
        };

    self.auth_tokens.write().await.insert(token.clone(), auth_token);

        Ok(token)
    }
}