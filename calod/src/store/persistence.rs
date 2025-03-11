use std::sync::Arc;

use super::{error::PersistenceError, sharded_store::ShardedStore};

#[async_trait::async_trait]
pub trait CachePersistence {
    async fn save(&self, path: &str) -> Result<(), PersistenceError>;
    async fn load(&self, path: &str) -> Result<(), PersistenceError>;
}

#[async_trait::async_trait]
impl CachePersistence for ShardedStore {
    async fn save(&self, path: &str) -> Result<(), PersistenceError> {
        let mut handles = Vec::new();

        for (i, shard) in self.get_all_shards().iter().enumerate() {
            let shard = Arc::clone(shard);
            let path = format!("{}/shard_{}.bin", path, i);
            handles.push(tokio::spawn(async move {
                shard.save(&path).await
            }));
        }

        for handle in handles {
            handle.await.map_err(|e| PersistenceError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Join error: {}", e)
            )))??;
        }

        Ok(())
    }
    
    async fn load(&self, path: &str) -> Result<(), PersistenceError> {
        for (i, shard) in self.get_all_shards().iter().enumerate() {
            let path = format!("{}/shard_{}.bin", path, i);
            shard.load(&path).await?;
        }
        Ok(())
    }
}