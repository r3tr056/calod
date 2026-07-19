use std::path::{Path, PathBuf};

use tokio::fs::{create_dir_all, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, Duration, MissedTickBehavior};

use crate::store::{config::FsyncStrategy, error::PersistenceError};

enum AofMessage {
    Append(Vec<u8>),
    Flush(oneshot::Sender<Result<(), PersistenceError>>),
}

#[derive(Clone)]
pub struct AofLogger {
    enabled: bool,
    tx: Option<mpsc::UnboundedSender<AofMessage>>,
    path: PathBuf,
}

impl AofLogger {
    pub fn disabled(dir: &Path) -> Self {
        Self {
            enabled: false,
            tx: None,
            path: dir.join("appendonly.aof"),
        }
    }

    pub async fn new(
        enabled: bool,
        dir: &Path,
        fsync_strategy: FsyncStrategy,
    ) -> Result<Self, PersistenceError> {
        let path = dir.join("appendonly.aof");
        if enabled {
            create_dir_all(dir).await.map_err(PersistenceError::Io)?;
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .map_err(PersistenceError::Io)?;
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                let _ = Self::run_writer(file, rx, fsync_strategy).await;
            });
            Ok(Self {
                enabled,
                tx: Some(tx),
                path,
            })
        } else {
            Ok(Self {
                enabled,
                tx: None,
                path,
            })
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub async fn append(&self, raw_resp_frame: &[u8]) -> Result<(), PersistenceError> {
        if !self.enabled {
            return Ok(());
        }
        let Some(tx) = &self.tx else {
            return Err(PersistenceError::AofWriteFailed(
                "AOF writer channel missing".to_string(),
            ));
        };
        tx.send(AofMessage::Append(raw_resp_frame.to_vec()))
            .map_err(|_| PersistenceError::AofWriteFailed("AOF writer task stopped".to_string()))
    }

    pub async fn flush(&self) -> Result<(), PersistenceError> {
        if !self.enabled {
            return Ok(());
        }
        let Some(tx) = &self.tx else {
            return Err(PersistenceError::AofWriteFailed(
                "AOF writer channel missing".to_string(),
            ));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(AofMessage::Flush(reply_tx))
            .map_err(|_| PersistenceError::AofWriteFailed("AOF writer task stopped".to_string()))?;
        reply_rx.await.map_err(|_| {
            PersistenceError::AofWriteFailed("AOF flush acknowledgment lost".to_string())
        })?
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn run_writer(
        mut file: tokio::fs::File,
        mut rx: mpsc::UnboundedReceiver<AofMessage>,
        fsync_strategy: FsyncStrategy,
    ) -> Result<(), PersistenceError> {
        let mut dirty = false;
        let mut everysec_tick = time::interval(Duration::from_secs(1));
        everysec_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                maybe_message = rx.recv() => {
                    let Some(message) = maybe_message else {
                        if dirty {
                            Self::flush_file(&mut file, !matches!(fsync_strategy, FsyncStrategy::No)).await?;
                        }
                        return Ok(());
                    };

                    let mut flush_waiters = Vec::new();
                    let mut force_sync = false;
                    Self::handle_message(&mut file, message, &mut flush_waiters, &mut dirty).await?;

                    while let Ok(message) = rx.try_recv() {
                        Self::handle_message(&mut file, message, &mut flush_waiters, &mut dirty).await?;
                    }

                    if matches!(fsync_strategy, FsyncStrategy::Always) && dirty {
                        force_sync = true;
                    }

                    if force_sync || !flush_waiters.is_empty() {
                        let sync_now = force_sync || !flush_waiters.is_empty();
                        let flush_result = Self::flush_file(&mut file, sync_now).await;
                        if flush_result.is_ok() {
                            dirty = false;
                        }
                        for waiter in flush_waiters {
                            let result = match &flush_result {
                                Ok(_) => Ok(()),
                                Err(err) => Err(PersistenceError::AofWriteFailed(err.to_string())),
                            };
                            let _ = waiter.send(result);
                        }
                        flush_result?;
                    }
                }
                _ = everysec_tick.tick(), if matches!(fsync_strategy, FsyncStrategy::Everysec) && dirty => {
                    Self::flush_file(&mut file, true).await?;
                    dirty = false;
                }
            }
        }
    }

    async fn handle_message(
        file: &mut tokio::fs::File,
        message: AofMessage,
        flush_waiters: &mut Vec<oneshot::Sender<Result<(), PersistenceError>>>,
        dirty: &mut bool,
    ) -> Result<(), PersistenceError> {
        match message {
            AofMessage::Append(frame) => {
                file.write_all(&frame).await.map_err(PersistenceError::Io)?;
                *dirty = true;
            }
            AofMessage::Flush(waiter) => {
                flush_waiters.push(waiter);
            }
        }
        Ok(())
    }

    async fn flush_file(
        file: &mut tokio::fs::File,
        sync_data: bool,
    ) -> Result<(), PersistenceError> {
        file.flush().await.map_err(PersistenceError::Io)?;
        if sync_data {
            file.sync_data().await.map_err(PersistenceError::Io)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use uuid::Uuid;

    use super::*;

    fn temp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("calod-aof-test-{}", Uuid::new_v4()));
        p
    }

    #[tokio::test]
    async fn buffered_writer_persists_frames_after_flush() {
        let dir = temp_dir();
        let logger = AofLogger::new(true, dir.as_path(), FsyncStrategy::Everysec)
            .await
            .expect("logger");

        logger
            .append(b"*1\r\n$4\r\nPING\r\n")
            .await
            .expect("append");
        logger
            .append(b"*1\r\n$4\r\nPING\r\n")
            .await
            .expect("append");
        logger.flush().await.expect("flush");

        let data = fs::read(logger.path()).expect("read AOF file");
        assert_eq!(data, b"*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nPING\r\n");

        let _ = fs::remove_dir_all(dir);
    }
}
