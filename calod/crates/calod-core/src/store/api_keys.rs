//! Scoped API-key store.
//!
//! API keys authenticate non-RESP clients (the REST/gRPC management APIs) and
//! carry an explicit scope set (e.g. `effects:read`, `effects:write`,
//! `admin`). The server never stores plaintext secrets: a key is created with
//! a one-time `cld_<random>` secret and only its argon2 hash is kept, exactly
//! like the RESP password path in `auth.rs`.
//!
//! # Persistence
//!
//! Keys live in `api_keys.bin` in the data directory:
//!
//! ```text
//! | magic "CLDK" (4B) | version (1B) | bincode payload | crc32c trailer (4B LE) |
//! ```
//!
//! The file is rewritten in full on every mutation (key counts are tiny). A
//! corrupt file fails closed: [`ApiKeyStore::open`] returns
//! [`PersistenceError::Corruption`] and startup refuses to proceed; the
//! `allow_data_loss` escape hatch quarantines the file via
//! [`ApiKeyStore::quarantine`] (rename, never delete) and starts empty.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error::PersistenceError;

/// File name of the API-key store inside the data directory.
pub const API_KEY_FILE_NAME: &str = "api_keys.bin";

const FILE_MAGIC: &[u8; 4] = b"CLDK";
const FILE_VERSION: u8 = 1;
const SECRET_PREFIX: &str = "cld_";
const SECRET_RANDOM_BYTES: usize = 32;

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A stored API key. `secret_hash` is the argon2 PHC string of the plaintext
/// secret; the plaintext itself is only ever returned once, at creation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    /// Key identifier (UUID v4).
    pub id: Uuid,
    pub tenant_id: String,
    pub project_id: String,
    /// Argon2 PHC string of the plaintext secret.
    pub secret_hash: String,
    /// Fast lookup index: XxHash64 of the plaintext secret, so `verify` can
    /// find the candidate record without an argon2 run per stored key.
    pub secret_fingerprint: u64,
    /// Granted scopes, e.g. `effects:read`, `effects:write`, `admin`.
    pub scopes: BTreeSet<String>,
    /// Seconds since the Unix epoch.
    pub created_at: u64,
    /// Optional expiry, seconds since the Unix epoch.
    pub expires_at: Option<u64>,
    pub revoked: bool,
}

/// Metadata-only view of an API key. This is the only shape handed out by
/// `list_by_project` (and, later, the REST API) — it never carries the secret
/// hash or fingerprint.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiKeyMetadata {
    pub id: Uuid,
    pub tenant_id: String,
    pub project_id: String,
    pub scopes: BTreeSet<String>,
    pub created_at: u64,
    pub expires_at: Option<u64>,
    pub revoked: bool,
}

impl From<&ApiKeyRecord> for ApiKeyMetadata {
    fn from(record: &ApiKeyRecord) -> Self {
        Self {
            id: record.id,
            tenant_id: record.tenant_id.clone(),
            project_id: record.project_id.clone(),
            scopes: record.scopes.clone(),
            created_at: record.created_at,
            expires_at: record.expires_at,
            revoked: record.revoked,
        }
    }
}

#[derive(Default)]
struct StoreInner {
    records: BTreeMap<Uuid, ApiKeyRecord>,
    by_fingerprint: HashMap<u64, Uuid>,
}

/// Persistent, scoped API-key store. Cheap to clone: state is shared.
pub struct ApiKeyStore {
    path: PathBuf,
    inner: RwLock<StoreInner>,
    argon2: Argon2<'static>,
}

impl ApiKeyStore {
    /// Load the key store from `data_dir`, creating nothing on disk yet. A
    /// missing file means an empty store; a corrupt file is
    /// [`PersistenceError::Corruption`] (fail-closed — the caller decides
    /// whether startup aborts or the file is quarantined).
    pub fn open(data_dir: &Path) -> Result<Self, PersistenceError> {
        let path = data_dir.join(API_KEY_FILE_NAME);
        let inner = match std::fs::read(&path) {
            Ok(bytes) => Self::decode(&bytes)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => StoreInner::default(),
            Err(err) => return Err(PersistenceError::Io(err)),
        };
        Ok(Self {
            path,
            inner: RwLock::new(inner),
            argon2: Argon2::default(),
        })
    }

    /// Move a corrupt `api_keys.bin` aside so a fresh store can be opened.
    /// The file is renamed to `api_keys.bin.corrupt-<unix-time>`, never
    /// deleted — same escape hatch as the WAL quarantine path.
    pub fn quarantine(data_dir: &Path) -> Result<Option<PathBuf>, PersistenceError> {
        let path = data_dir.join(API_KEY_FILE_NAME);
        if !path.exists() {
            return Ok(None);
        }
        let target = data_dir.join(format!("{}.corrupt-{}", API_KEY_FILE_NAME, now_unix_secs()));
        std::fs::rename(&path, &target).map_err(PersistenceError::Io)?;
        tracing::warn!(
            "API-key store quarantine: moved unrecoverable {} to {}",
            path.display(),
            target.display()
        );
        Ok(Some(target))
    }

    /// Create a key. Returns the metadata and the ONE-TIME plaintext secret
    /// (`cld_<random>`); only the argon2 hash is persisted.
    pub fn create(
        &self,
        tenant_id: &str,
        project_id: &str,
        scopes: BTreeSet<String>,
        expires_at: Option<u64>,
    ) -> Result<(ApiKeyMetadata, String), PersistenceError> {
        let mut random = [0u8; SECRET_RANDOM_BYTES];
        rand::rng().fill_bytes(&mut random);
        let secret = format!("{}{}", SECRET_PREFIX, hex_encode(&random));

        let mut salt_bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut salt_bytes);
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|err| PersistenceError::Corruption(format!("salt encode failed: {err}")))?;
        let secret_hash = self
            .argon2
            .hash_password(secret.as_bytes(), &salt)
            .map_err(|err| PersistenceError::Corruption(format!("argon2 hash failed: {err}")))?
            .to_string();

        let record = ApiKeyRecord {
            id: Uuid::new_v4(),
            tenant_id: tenant_id.to_string(),
            project_id: project_id.to_string(),
            secret_hash,
            secret_fingerprint: fingerprint(&secret),
            scopes,
            created_at: now_unix_secs(),
            expires_at,
            revoked: false,
        };
        let metadata = ApiKeyMetadata::from(&record);

        {
            let mut inner = self
                .inner
                .write()
                .map_err(|_| PersistenceError::Corruption("api-key store lock poisoned".into()))?;
            inner
                .by_fingerprint
                .insert(record.secret_fingerprint, record.id);
            inner.records.insert(record.id, record);
            self.persist_locked(&inner)?;
        }

        Ok((metadata, secret))
    }

    /// Verify a plaintext secret. Returns the key's metadata (with scopes)
    /// only when the hash matches (constant-time comparison inside argon2),
    /// the key is not revoked, and it has not expired. Any failure — unknown,
    /// wrong secret, expired, revoked — is indistinguishable `None`.
    pub fn verify(&self, secret: &str) -> Option<ApiKeyMetadata> {
        if !secret.starts_with(SECRET_PREFIX) {
            return None;
        }
        let inner = self.inner.read().ok()?;
        let id = inner.by_fingerprint.get(&fingerprint(secret))?;
        let record = inner.records.get(id)?;

        if record.revoked {
            return None;
        }
        if let Some(expires_at) = record.expires_at {
            if now_unix_secs() >= expires_at {
                return None;
            }
        }

        let parsed = PasswordHash::new(&record.secret_hash).ok()?;
        self.argon2
            .verify_password(secret.as_bytes(), &parsed)
            .ok()?;

        Some(ApiKeyMetadata::from(record))
    }

    /// Revoke a key by id. Returns false when the id is unknown.
    pub fn revoke(&self, id: Uuid) -> Result<bool, PersistenceError> {
        let mut inner = self
            .inner
            .write()
            .map_err(|_| PersistenceError::Corruption("api-key store lock poisoned".into()))?;
        let Some(record) = inner.records.get_mut(&id) else {
            return Ok(false);
        };
        record.revoked = true;
        self.persist_locked(&inner)?;
        Ok(true)
    }

    /// List keys for a project, metadata only — never secrets or hashes.
    pub fn list_by_project(&self, project_id: &str) -> Vec<ApiKeyMetadata> {
        let Ok(inner) = self.inner.read() else {
            return Vec::new();
        };
        inner
            .records
            .values()
            .filter(|record| record.project_id == project_id)
            .map(ApiKeyMetadata::from)
            .collect()
    }

    /// Rewrite the on-disk file with the current records (tmp + rename so a
    /// crash mid-write cannot leave a truncated store).
    fn persist_locked(&self, inner: &StoreInner) -> Result<(), PersistenceError> {
        let records: Vec<&ApiKeyRecord> = inner.records.values().collect();
        let payload = bincode::serialize(&records).map_err(PersistenceError::Serialization)?;

        let mut out = Vec::with_capacity(payload.len() + 9);
        out.extend_from_slice(FILE_MAGIC);
        out.push(FILE_VERSION);
        out.extend_from_slice(&payload);
        out.extend_from_slice(&crc32c::crc32c(&out).to_le_bytes());

        let tmp = self.path.with_extension("bin.tmp");
        std::fs::write(&tmp, &out).map_err(PersistenceError::Io)?;
        std::fs::rename(&tmp, &self.path).map_err(PersistenceError::Io)?;
        Ok(())
    }

    fn decode(bytes: &[u8]) -> Result<StoreInner, PersistenceError> {
        const HEADER_LEN: usize = FILE_MAGIC.len() + 1;
        if bytes.len() < HEADER_LEN + 4 {
            return Err(PersistenceError::Corruption(
                "api-keys file too short".into(),
            ));
        }
        if &bytes[..FILE_MAGIC.len()] != FILE_MAGIC {
            return Err(PersistenceError::Corruption(
                "api-keys file has bad magic".into(),
            ));
        }
        if bytes[FILE_MAGIC.len()] != FILE_VERSION {
            return Err(PersistenceError::Corruption(format!(
                "unsupported api-keys file version {}",
                bytes[FILE_MAGIC.len()]
            )));
        }

        let (body, crc_bytes) = bytes.split_at(bytes.len() - 4);
        let expected = u32::from_le_bytes(crc_bytes.try_into().unwrap());
        if crc32c::crc32c(body) != expected {
            return Err(PersistenceError::Corruption(
                "api-keys file checksum mismatch".into(),
            ));
        }

        let records: Vec<ApiKeyRecord> =
            bincode::deserialize(&body[HEADER_LEN..]).map_err(PersistenceError::Serialization)?;

        let mut inner = StoreInner::default();
        for record in records {
            inner
                .by_fingerprint
                .insert(record.secret_fingerprint, record.id);
            inner.records.insert(record.id, record);
        }
        Ok(inner)
    }
}

/// Fast, non-cryptographic lookup key for the secret. The argon2 verification
/// remains the actual authentication check; this only avoids hashing every
/// candidate on each verify.
fn fingerprint(secret: &str) -> u64 {
    use std::hash::Hasher;
    let mut hasher = twox_hash::XxHash64::default();
    hasher.write(secret.as_bytes());
    hasher.finish()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("calod-api-keys-{}-{}", name, Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scopes(pairs: &[&str]) -> BTreeSet<String> {
        pairs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn create_verify_round_trip() {
        let dir = test_dir("round-trip");
        let store = ApiKeyStore::open(&dir).unwrap();

        let (meta, secret) = store
            .create("tenant-1", "proj-1", scopes(&["effects:read"]), None)
            .unwrap();
        assert!(secret.starts_with("cld_"));
        assert_eq!(meta.project_id, "proj-1");
        assert!(meta.scopes.contains("effects:read"));
        assert!(!meta.revoked);

        let verified = store.verify(&secret).expect("secret must verify");
        assert_eq!(verified, meta);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let dir = test_dir("wrong-secret");
        let store = ApiKeyStore::open(&dir).unwrap();

        let (_meta, secret) = store
            .create("tenant-1", "proj-1", scopes(&["admin"]), None)
            .unwrap();

        assert!(store.verify("cld_0000deadbeef").is_none());
        let mut mangled = secret.clone();
        mangled.replace_range(4..5, if &secret[4..5] == "0" { "1" } else { "0" });
        assert!(store.verify(&mangled).is_none());
        // Secrets without the expected prefix never reach the hash check.
        assert!(store.verify("no-prefix").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn expired_key_is_rejected() {
        let dir = test_dir("expired");
        let store = ApiKeyStore::open(&dir).unwrap();

        let past = now_unix_secs().saturating_sub(1);
        let (_meta, secret) = store
            .create("tenant-1", "proj-1", scopes(&["effects:read"]), Some(past))
            .unwrap();

        assert!(store.verify(&secret).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn revoked_key_is_rejected() {
        let dir = test_dir("revoked");
        let store = ApiKeyStore::open(&dir).unwrap();

        let (meta, secret) = store
            .create("tenant-1", "proj-1", scopes(&["effects:write"]), None)
            .unwrap();
        assert!(store.verify(&secret).is_some());

        assert!(store.revoke(meta.id).unwrap());
        assert!(store.verify(&secret).is_none());
        // Revoking an unknown id is a no-op, not an error.
        assert!(!store.revoke(Uuid::new_v4()).unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persistence_round_trip_survives_reopen() {
        let dir = test_dir("persistence");

        let (meta, secret) = {
            let store = ApiKeyStore::open(&dir).unwrap();
            store
                .create(
                    "tenant-1",
                    "proj-1",
                    scopes(&["effects:read", "effects:write"]),
                    None,
                )
                .unwrap()
        };
        assert!(dir.join(API_KEY_FILE_NAME).exists());

        let reopened = ApiKeyStore::open(&dir).unwrap();
        let verified = reopened
            .verify(&secret)
            .expect("secret must survive reopen");
        assert_eq!(verified, meta);

        // Revocation is persisted too.
        reopened.revoke(meta.id).unwrap();
        let reopened_again = ApiKeyStore::open(&dir).unwrap();
        assert!(reopened_again.verify(&secret).is_none());
        assert!(reopened_again.list_by_project("proj-1")[0].revoked);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_file_fails_closed_and_quarantine_moves_it_aside() {
        let dir = test_dir("corrupt");
        let store = ApiKeyStore::open(&dir).unwrap();
        store
            .create("tenant-1", "proj-1", scopes(&["admin"]), None)
            .unwrap();

        // Flip a payload byte so the CRC no longer matches.
        let path = dir.join(API_KEY_FILE_NAME);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[8] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let err = ApiKeyStore::open(&dir)
            .err()
            .expect("corrupt file must fail closed");
        assert!(matches!(err, PersistenceError::Corruption(_)));

        let quarantined = ApiKeyStore::quarantine(&dir).unwrap().expect("file moved");
        assert!(!path.exists());
        assert!(quarantined.exists());
        // After quarantine a fresh store opens empty.
        let fresh = ApiKeyStore::open(&dir).unwrap();
        assert!(fresh.list_by_project("proj-1").is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn metadata_listings_never_expose_secrets_or_hashes() {
        let dir = test_dir("metadata");
        let store = ApiKeyStore::open(&dir).unwrap();

        let (meta, secret) = store
            .create("tenant-1", "proj-1", scopes(&["admin"]), None)
            .unwrap();
        store
            .create("tenant-1", "proj-2", scopes(&["effects:read"]), None)
            .unwrap();

        let listed = store.list_by_project("proj-1");
        assert_eq!(listed, vec![meta]);

        let json = serde_json::to_string(&listed).unwrap();
        assert!(!json.contains("hash"), "metadata must not carry hashes");
        assert!(!json.contains("secret"), "metadata must not carry secrets");
        assert!(
            !json.contains(&secret),
            "plaintext secret must never appear"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
