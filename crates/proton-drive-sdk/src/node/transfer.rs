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
        assert!(
            max_degree_of_parallelism > 0,
            "transfer parallelism must be greater than zero"
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_capacity_is_released_with_the_permit() {
        let queue = TransferQueue::new(2);
        let permit = queue.start_file().await.unwrap();
        assert!(queue.file_semaphore.try_acquire().is_err());
        drop(permit);
        assert!(queue.file_semaphore.try_acquire().is_ok());
    }

    #[tokio::test]
    async fn block_capacity_matches_configured_parallelism() {
        let queue = TransferQueue::new(2);
        let first = queue.start_block().await.unwrap();
        let second = queue.start_block().await.unwrap();
        assert!(queue.block_semaphore.try_acquire().is_err());
        drop(first);
        assert!(queue.block_semaphore.try_acquire().is_ok());
        drop(second);
    }

    #[tokio::test]
    async fn cloned_queues_share_capacity() {
        let queue = TransferQueue::new(1);
        let clone = queue.clone();
        let permit = queue.start_block().await.unwrap();
        assert!(clone.block_semaphore.try_acquire().is_err());
        drop(permit);
        assert!(clone.start_block().await.is_ok());
    }

    #[test]
    #[should_panic(expected = "transfer parallelism must be greater than zero")]
    fn rejects_zero_parallelism() {
        let _ = TransferQueue::new(0);
    }
}
