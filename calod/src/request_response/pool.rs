use std::sync::Arc;
use bb8::{ManageConnection, Pool as Bb8Pool, RunError};
use tokio::sync::Mutex;

use super::{served_store::ServedCalodStore, server::Connection};

#[derive(thiserror::Error, Debug)]
pub enum CalodError {
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
    
    #[error("Connection pool error: {0}")]
    PoolError(#[from] RunError<std::io::Error>),
}

pub struct CalodConnectionManager {
    store: Arc<ServedCalodStore>,
}

impl CalodConnectionManager {
    pub fn new(store: Arc<ServedCalodStore>) -> Self {
        Self { store }
    }
}

impl ManageConnection for CalodConnectionManager {
    type Connection = Mutex<Connection>;
    type Error = std::io::Error;

    async fn connect(&self) -> Result<Self::Connection, Self::Error> {
        Ok(Mutex::new(Connection::new(self.store.clone())))
    }

    async fn is_valid(&self, _conn: &mut Self::Connection) -> Result<(), Self::Error> {
        Ok(())
    }

    fn has_broken(&self, _conn: &mut Self::Connection) -> bool {
        false
    }
}

#[derive(Clone)]
pub struct Pool {
    inner: Arc<Bb8Pool<CalodConnectionManager>>,
}

impl Pool {
    pub async fn new(size: u32, manager: CalodConnectionManager) -> Result<Self, CalodError> {
        let pool = Bb8Pool::builder()
            .max_size(size)
            .build(manager)
            .await?;
            
        Ok(Self {
            inner: Arc::new(pool),
        })
    }

    pub async fn get(&self) -> Result<bb8::PooledConnection<'_, CalodConnectionManager>, CalodError> {
        self.inner.get().await.map_err(CalodError::from)
    }
}

pub async fn create_connection_pool() -> Result<Pool, CalodError> {
    let store = Arc::new(ServedCalodStore::new());
    let manager = CalodConnectionManager::new(store);
    Pool::new(num_cpus::get() as u32, manager).await
}