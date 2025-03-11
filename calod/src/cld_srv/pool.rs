use std::{sync::Arc, time::Duration};
use std::io::Error;
use ahash::RandomState;
use bb8::{ManageConnection, Pool as Bb8Pool, RunError};
use dashmap::DashMap;
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

use super::{auth::AuthManager, connection::{Connection, ConnectionConfig}, metrics::MetricsAggregator, served_store::ServedCalodStore};

#[derive(thiserror::Error, Debug)]
pub enum CalodError {
    #[error("I/O error: {0}")]
    IoError(#[from] Error),
    
    #[error("Connection pool error: {0}")]
    PoolError(String),

    #[error("Connection limit reached")]
    ConnectionLimitReached,
    
    #[error("Operation timed out: {0}")]
    Timeout(String),
}

impl From<RunError<Error>> for CalodError {
    fn from(err: RunError<Error>) -> Self {
        CalodError::PoolError(err.to_string())
    }
}

/// Pool configuration
#[derive(Clone)]
pub struct PoolConfig {
    pub min_idle: u32,
    pub max_size: u32,
    pub max_lifetime_secs: u64,
    pub idle_timeout_secs: u64,
    pub connection_timeout_ms: u64,
    pub max_connections: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        let num_cpus = num_cpus::get();
        Self {
            min_idle: 10,
            max_size: (num_cpus * 4) as u32,  // 4x CPU count for optimal parallelism
            max_lifetime_secs: 3600,          // 1 hour max connection lifetime
            idle_timeout_secs: 300,           // 5 minutes idle timeout 
            connection_timeout_ms: 5000,      // 5 seconds connection timeout
            max_connections: 100_000,         // Support up to 100K connections (Redis scale)
        }
    }
}

pub struct CalodConnectionManager {
    store: Arc<ServedCalodStore>,
    config: ConnectionConfig,
    metrics: Arc<MetricsAggregator>,
    auth_manager: Arc<AuthManager>,
}

impl CalodConnectionManager {
    pub fn new(store: Arc<ServedCalodStore>, config: ConnectionConfig, metrics: Arc<MetricsAggregator>, auth_manager: Arc<AuthManager>) -> Self {
        Self {
            store,
            config,
            metrics,
            auth_manager
        }
    }
}

impl ManageConnection for CalodConnectionManager {
    type Connection = Mutex<Connection>;
    type Error = std::io::Error;

    async fn connect(&self) -> Result<Self::Connection, Self::Error> {
        let conn = Connection::new(
            self.store.clone(),
            self.config.clone(),
            self.metrics.clone(),
            self.auth_manager.clone(),
        );
        Ok(Mutex::new(conn))
    }

    async fn is_valid(&self, conn: &mut Self::Connection) -> Result<(), Self::Error> {
        let connection = conn.lock().await;
        if connection.get_uptime() > Duration::from_secs(3600) {
            return Err(Error::new(std::io::ErrorKind::TimedOut, "Connection too old"));
        }

        Ok(())
    }

    fn has_broken(&self, _conn: &mut Self::Connection) -> bool {
        false
    }
}

#[derive(Clone)]
pub struct Pool {
    inner: Arc<Bb8Pool<CalodConnectionManager>>,
    conn_semaphore: Arc<Semaphore>, // For global connection limiting
    active_connections: Arc<DashMap<Uuid, Arc<Mutex<Connection>>, RandomState>>,
    config: PoolConfig,
    metrics: Arc<MetricsAggregator>,
}

impl Pool {
    pub async fn new(
        pool_config: PoolConfig,
        connection_config: ConnectionConfig,
        auth_manager: Arc<AuthManager>,
        metrics: Arc<MetricsAggregator>,
    ) -> Result<Self, CalodError> {
        let store = Arc::new(ServedCalodStore::new());
        let manager = CalodConnectionManager::new(store, connection_config, metrics.clone(), auth_manager);

        // Setup connection semaphore to limit total connections
        let conn_semaphore = Arc::new(Semaphore::new(pool_config.max_connections));
        
        // Setup connection tracking with optimized DashMap
        let active_connections: Arc<DashMap<Uuid, Arc<Mutex<Connection>>, RandomState>>= Arc::new(DashMap::with_capacity_and_hasher(1024, ahash::RandomState::new()));


        let pool = Bb8Pool::builder()
            .max_size(pool_config.max_size)
            .min_idle(Some(pool_config.min_idle))
            .max_lifetime(Some(Duration::from_secs(pool_config.max_lifetime_secs)))
            .idle_timeout(Some(Duration::from_secs(pool_config.idle_timeout_secs)))
            .connection_timeout(Duration::from_millis(pool_config.connection_timeout_ms))
            .build(manager)
            .await?;
            
        Ok(Self {
            inner: Arc::new(pool),
            conn_semaphore,
            active_connections,
            config: pool_config,
            metrics,
        })
    }

    /// Acquire a connection with timeout
    #[inline]
    pub async fn get(&self) -> Result<bb8::PooledConnection<'_, CalodConnectionManager>, CalodError> {
        if let Ok(permit) = self.conn_semaphore.acquire().await {
            permit.forget();

            match self.inner.get().await {
                Ok(conn) => Ok(conn),
                Err(e) => {
                    self.conn_semaphore.add_permits(1);
                    Err(CalodError::from(e))
                }
            }
        } else {
            Err(CalodError::ConnectionLimitReached)
        }
    }

    /// Register an active connection for tracking
    #[inline]
    pub fn register_connection(&self, conn: Arc<Mutex<Connection>>) {
        let client_id = {
            let blocking = tokio::task::block_in_place(|| {
                let guard = conn.blocking_lock();
                guard.get_client_id()
            });
            blocking
        };

        self.active_connections.insert(client_id, conn.clone());
        self.metrics.connections_total_inc();
    }

    /// Unregister a connection that's closing
    #[inline]
    pub fn unregister_connection(&self, client_id: Uuid) {
        self.active_connections.remove(&client_id);
        self.conn_semaphore.add_permits(1);
        self.metrics.connections_total_dec();
    }

    /// Get connection semaphore for limiting connections
    #[inline]
    pub fn connection_semaphore(&self) -> Arc<Semaphore> {
        self.conn_semaphore.clone()
    }
}

pub async fn create_connection_pool() -> Result<Pool, CalodError> {
    let auth_manager = Arc::new(AuthManager::new(false));
    let metrics = Arc::new(MetricsAggregator::new());

    Pool::new(PoolConfig::default(), ConnectionConfig::default(), auth_manager, metrics).await
}

/// Create secure connection pool with authentication
pub async fn create_secure_connection_pool(password: Option<&str>) -> Result<Pool, CalodError> {
    let auth_required = password.is_some();
    let auth_manager = Arc::new(AuthManager::new(auth_required));

    if let Some(pass) = password {
        auth_manager.add_password(pass);
    }
    
    let metrics = Arc::new(MetricsAggregator::new());
    
    Pool::new(
        PoolConfig::default(),
        ConnectionConfig::default(),
        auth_manager,
        metrics
    ).await
}