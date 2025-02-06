use std::sync::Arc;

use bb8::{Pool, PooledConnection};
use crate::request_response::server::Connection;

use super::served_store::ServedCalodStore;

pub type ConnectionPool = Pool<ConnectionManager>;
pub type PooledConn = PooledConnection<'static, ConnectionManager>;

pub struct ConnectionManager {
    store: Arc<ServedCalodStore>,
}

impl bb8::ManageConnection for ConnectionManager {
    type Connection = Connection;
    type Error = std::io::Error;

    async fn connect(&self) -> Result<Self::Connection, Self::Error> {
        Ok(Connection::new(self.store.clone()))
    }

    async fn is_valid(&self, _: &mut Self::Connection) -> Result<(), Self::Error> {
        Ok(())
    }

    fn has_broken(&self, _: &mut Self::Connection) -> bool {
        false
    }
}

pub async fn create_connection_pool() -> Result<ConnectionPool, Box<dyn std::error::Error>> {
    let store = Arc::new(ServedCalodStore::new());
    let mgr = ConnectionManager { store };

    Pool::builder().max_size(1000).min_idle(Some(100)).build(mgr).await.map_err(Into::into)
}