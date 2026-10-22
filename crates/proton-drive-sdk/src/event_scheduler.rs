//! Schedules event polling per tree scope, mirroring upstream's `EventScheduler`.
//!
//! The own volume is always polled in the foreground. At most one shared scope
//! is in the foreground at a time; the others are polled in the background.
//! The next poll is scheduled only after the callback finishes, so the client
//! is never called again before it has processed the previous events.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::mpsc;

const FOREGROUND_POLLING_INTERVAL: Duration = Duration::from_secs(30);
const BACKGROUND_POLLING_INTERVAL: Duration = Duration::from_secs(10 * 60);
const JITTER: Duration = Duration::from_secs(1);

type Callback = Arc<dyn Fn(String) -> BoxFuture<'static, ()> + Send + Sync>;

enum Command {
    /// Poll now, then restart the timer.
    PollNow,
    /// Restart the timer with the current interval, without polling.
    Reschedule,
}

struct Scope {
    is_own_volume: bool,
    is_foreground: Arc<AtomicBool>,
    commands: mpsc::UnboundedSender<Command>,
}

/// Polls each added scope by calling the callback with its tree event scope ID.
///
/// Polling runs on the current tokio runtime and stops when a scope is removed
/// or the scheduler is dropped.
pub struct EventScheduler {
    callback: Callback,
    own_volume_id: String,
    intervals: (Duration, Duration, Duration),
    scopes: Mutex<HashMap<String, Scope>>,
}

impl EventScheduler {
    pub fn new<F, Fut>(own_volume_id: impl Into<String>, callback: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::with_intervals(
            own_volume_id,
            callback,
            (
                FOREGROUND_POLLING_INTERVAL,
                BACKGROUND_POLLING_INTERVAL,
                JITTER,
            ),
        )
    }

    fn with_intervals<F, Fut>(
        own_volume_id: impl Into<String>,
        callback: F,
        intervals: (Duration, Duration, Duration),
    ) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self {
            callback: Arc::new(move |scope| Box::pin(callback(scope))),
            own_volume_id: own_volume_id.into(),
            intervals,
            scopes: Mutex::new(HashMap::new()),
        }
    }

    /// Starts polling a scope, polling it right away to get the initial events.
    pub fn add_scope(&self, event_tree_scope_id: impl Into<String>) {
        let id = event_tree_scope_id.into();
        let mut scopes = self.scopes.lock().unwrap();
        if scopes.contains_key(&id) {
            return;
        }
        let is_own_volume = id == self.own_volume_id;
        let is_foreground = Arc::new(AtomicBool::new(is_own_volume));
        let (commands, receiver) = mpsc::unbounded_channel();
        tokio::spawn(poll_loop(
            id.clone(),
            self.callback.clone(),
            is_foreground.clone(),
            self.intervals,
            receiver,
        ));
        scopes.insert(
            id,
            Scope {
                is_own_volume,
                is_foreground,
                commands,
            },
        );
    }

    /// Moves a shared scope to the foreground, sending any other foreground shared
    /// scope to the background, and polls it right away.
    pub fn set_foreground(&self, event_tree_scope_id: &str) {
        let scopes = self.scopes.lock().unwrap();
        let Some(scope) = scopes.get(event_tree_scope_id) else {
            return;
        };
        if scope.is_own_volume || scope.is_foreground.load(Ordering::SeqCst) {
            return;
        }
        for other in scopes.values() {
            if !other.is_own_volume && other.is_foreground.swap(false, Ordering::SeqCst) {
                let _ = other.commands.send(Command::Reschedule);
            }
        }
        scope.is_foreground.store(true, Ordering::SeqCst);
        let _ = scope.commands.send(Command::PollNow);
    }

    /// Moves a shared scope back to background polling, without polling it.
    pub fn set_background(&self, event_tree_scope_id: &str) {
        let scopes = self.scopes.lock().unwrap();
        let Some(scope) = scopes.get(event_tree_scope_id) else {
            return;
        };
        if scope.is_own_volume || !scope.is_foreground.swap(false, Ordering::SeqCst) {
            return;
        }
        let _ = scope.commands.send(Command::Reschedule);
    }

    /// Stops polling a scope.
    pub fn remove_scope(&self, event_tree_scope_id: &str) {
        // Dropping the sender ends the scope's polling task.
        self.scopes.lock().unwrap().remove(event_tree_scope_id);
    }
}

async fn poll_loop(
    id: String,
    callback: Callback,
    is_foreground: Arc<AtomicBool>,
    (foreground, background, jitter): (Duration, Duration, Duration),
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let mut poll = true;
    loop {
        if poll {
            callback(id.clone()).await;
        }
        let interval = if is_foreground.load(Ordering::SeqCst) {
            foreground
        } else {
            background
        };
        let delay = interval + jitter.mul_f64(rand::random::<f64>());
        tokio::select! {
            _ = tokio::time::sleep(delay) => poll = true,
            command = commands.recv() => match command {
                Some(Command::PollNow) => poll = true,
                Some(Command::Reschedule) => poll = false,
                None => return,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording_scheduler(
        intervals: (Duration, Duration, Duration),
    ) -> (EventScheduler, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let scheduler = EventScheduler::with_intervals(
            "own",
            move |scope| {
                recorded.lock().unwrap().push(scope);
                async {}
            },
            intervals,
        );
        (scheduler, calls)
    }

    fn count(calls: &Mutex<Vec<String>>, scope: &str) -> usize {
        calls.lock().unwrap().iter().filter(|s| *s == scope).count()
    }

    #[tokio::test]
    async fn scopes_poll_immediately_then_by_foreground_and_background_intervals() {
        let (scheduler, calls) = recording_scheduler((
            Duration::from_millis(40),
            Duration::from_secs(60),
            Duration::ZERO,
        ));
        scheduler.add_scope("own");
        scheduler.add_scope("shared");
        scheduler.add_scope("own");
        tokio::time::sleep(Duration::from_millis(150)).await;

        // The own volume keeps polling in the foreground; the shared scope polled once.
        assert!(count(&calls, "own") >= 3);
        assert_eq!(count(&calls, "shared"), 1);

        scheduler.set_foreground("shared");
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(count(&calls, "shared"), 2);

        scheduler.set_background("shared");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(count(&calls, "shared"), 2);

        scheduler.remove_scope("own");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let own = count(&calls, "own");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(count(&calls, "own"), own);
    }

    #[tokio::test]
    async fn only_one_shared_scope_stays_in_the_foreground() {
        let (scheduler, calls) = recording_scheduler((
            Duration::from_millis(40),
            Duration::from_secs(60),
            Duration::ZERO,
        ));
        scheduler.add_scope("a");
        scheduler.add_scope("b");
        scheduler.set_foreground("a");
        scheduler.set_foreground("b");
        tokio::time::sleep(Duration::from_millis(150)).await;

        // "a" was sent back to the background when "b" came to the foreground.
        assert_eq!(count(&calls, "a"), 2);
        assert!(count(&calls, "b") >= 4);
    }
}
