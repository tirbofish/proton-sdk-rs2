use std::collections::VecDeque;
use tokio::sync::Notify;
use std::sync::Arc;
use parking_lot::Mutex;

#[derive(Debug, Clone)]
pub struct FifoFlexibleSemaphore {
    inner: Arc<SemaphoreInner>,
}

#[derive(Debug)]
struct SemaphoreInner {
    max_permits: usize,
    state: Mutex<SemaphoreState>,
}

#[derive(Debug)]
struct SemaphoreState {
    current_count: i32,
    waiting_queue: VecDeque<(usize, Arc<Notify>)>,
}

impl FifoFlexibleSemaphore {
    pub fn new(max_permits: usize) -> Self {
        Self {
            inner: Arc::new(SemaphoreInner {
                max_permits,
                state: Mutex::new(SemaphoreState {
                    current_count: max_permits as i32,
                    waiting_queue: VecDeque::new(),
                }),
            }),
        }
    }

    pub fn max_permits(&self) -> usize {
        self.inner.max_permits
    }

    pub async fn acquire(&self, count: usize) -> anyhow::Result<()> {
        let notify = {
            let mut state = self.inner.state.lock();
            if state.current_count > 0 {
                state.current_count -= count as i32;
                return Ok(());
            }
            let notify = Arc::new(Notify::new());
            state.waiting_queue.push_back((count, notify.clone()));
            notify
        };

        notify.notified().await;
        Ok(())
    }

    pub fn release(&self, count: usize) {
        let mut state = self.inner.state.lock();
        state.current_count += count as i32;
        if state.current_count > self.inner.max_permits as i32 {
            state.current_count = self.inner.max_permits as i32;
        }

        while state.current_count > 0 {
            if let Some((count_to_decrement, notify)) = state.waiting_queue.pop_front() {
                state.current_count -= count_to_decrement as i32;
                notify.notify_one();
            } else {
                break;
            }
        }
    }
}
