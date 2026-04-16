use std::future::Future;

use tokio::sync::oneshot;

/// A lightweight bridge between async tasks and egui's immediate-mode loop.
///
/// Spawn a future with [`AsyncTask::spawn`], then poll with [`AsyncTask::poll`] each frame.
/// When the future completes the result is returned exactly once.
pub struct AsyncTask<T> {
    rx: oneshot::Receiver<T>,
}

impl<T: Send + 'static> AsyncTask<T> {
    /// Spawns a future on the tokio runtime and returns a pollable handle.
    pub fn spawn(rt: &tokio::runtime::Handle, future: impl Future<Output = T> + Send + 'static) -> Self {
        let (tx, rx) = oneshot::channel();
        rt.spawn(async move {
            let result = future.await;
            let _ = tx.send(result);
        });
        Self { rx }
    }

    /// Non-blocking poll. Returns `Some(result)` exactly once when the task completes.
    pub fn poll(&mut self) -> Option<T> {
        match self.rx.try_recv() {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}
