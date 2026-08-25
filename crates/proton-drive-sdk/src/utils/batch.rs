use async_trait::async_trait;
use std::marker::PhantomData;

#[async_trait]
pub trait BatchLoader<TId, TValue>: Send + Sync {
    async fn load_batch(&self, ids: Vec<TId>) -> anyhow::Result<Vec<TValue>>;
}

pub struct BatchLoaderBase<TId, TValue, L>
where
    L: BatchLoader<TId, TValue>,
{
    loader: L,
    queue: Vec<TId>,
    batch_size: usize,
    _phantom: PhantomData<TValue>,
}

impl<TId, TValue, L> BatchLoaderBase<TId, TValue, L>
where
    L: BatchLoader<TId, TValue>,
    TId: Clone,
{
    pub fn new(loader: L, batch_size: usize) -> Self {
        assert!(batch_size > 0, "batch size must be greater than zero");
        Self {
            loader,
            queue: Vec::with_capacity(batch_size),
            batch_size,
            _phantom: PhantomData,
        }
    }

    pub async fn queue_and_try_load_batch(&mut self, id: TId) -> anyhow::Result<Vec<TValue>> {
        self.queue.push(id);

        if self.queue.len() < self.batch_size {
            return Ok(Vec::new());
        }

        self.load_queued_batch().await
    }

    pub async fn load_remaining(&mut self) -> anyhow::Result<Vec<TValue>> {
        if self.queue.is_empty() {
            return Ok(Vec::new());
        }

        self.load_queued_batch().await
    }

    async fn load_queued_batch(&mut self) -> anyhow::Result<Vec<TValue>> {
        let ids = std::mem::replace(&mut self.queue, Vec::with_capacity(self.batch_size));
        self.loader.load_batch(ids).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[derive(Clone)]
    struct RecordingLoader(Arc<Mutex<Vec<Vec<u8>>>>);

    #[async_trait]
    impl BatchLoader<u8, u8> for RecordingLoader {
        async fn load_batch(&self, ids: Vec<u8>) -> anyhow::Result<Vec<u8>> {
            self.0.lock().push(ids.clone());
            Ok(ids)
        }
    }

    #[tokio::test]
    async fn loads_exact_batches_and_the_remainder() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut loader = BatchLoaderBase::new(RecordingLoader(calls.clone()), 3);

        assert!(loader.queue_and_try_load_batch(1).await.unwrap().is_empty());
        assert!(loader.queue_and_try_load_batch(2).await.unwrap().is_empty());
        assert_eq!(
            loader.queue_and_try_load_batch(3).await.unwrap(),
            vec![1, 2, 3]
        );
        assert!(loader.queue_and_try_load_batch(4).await.unwrap().is_empty());
        assert_eq!(loader.load_remaining().await.unwrap(), vec![4]);
        assert_eq!(*calls.lock(), vec![vec![1, 2, 3], vec![4]]);
    }

    #[tokio::test]
    async fn supports_single_and_oversized_batches() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut single = BatchLoaderBase::new(RecordingLoader(calls.clone()), 1);
        assert_eq!(single.queue_and_try_load_batch(1).await.unwrap(), vec![1]);
        assert_eq!(single.queue_and_try_load_batch(2).await.unwrap(), vec![2]);

        let mut oversized = BatchLoaderBase::new(RecordingLoader(calls), 10);
        assert!(
            oversized
                .queue_and_try_load_batch(3)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(oversized.load_remaining().await.unwrap(), vec![3]);
        assert!(oversized.load_remaining().await.unwrap().is_empty());
    }

    #[test]
    #[should_panic(expected = "batch size must be greater than zero")]
    fn rejects_zero_batch_size() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let _ = BatchLoaderBase::<u8, u8, _>::new(RecordingLoader(calls), 0);
    }
}
