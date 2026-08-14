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
