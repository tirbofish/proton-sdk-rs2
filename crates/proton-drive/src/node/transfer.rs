use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone)]
pub struct TransferQueue {
    file_semaphore: Arc<Semaphore>,
    block_semaphore: Arc<Semaphore>,
    #[allow(dead_code)]
    max_degree_of_parallelism: usize,
}

impl TransferQueue {
    pub fn new(max_degree_of_parallelism: usize) -> Self {
        Self {
            file_semaphore: Arc::new(Semaphore::new(1)),
            block_semaphore: Arc::new(Semaphore::new(max_degree_of_parallelism)),
            max_degree_of_parallelism,
        }
    }

    pub async fn start_file(&self) -> anyhow::Result<OwnedSemaphorePermit> {
        let permit = self.file_semaphore.clone().acquire_owned().await?;
        Ok(permit)
    }

    pub async fn start_block(&self) -> anyhow::Result<OwnedSemaphorePermit> {
        let permit = self.block_semaphore.clone().acquire_owned().await?;
        Ok(permit)
    }
}
