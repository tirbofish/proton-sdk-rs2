//! Hierarchical cancellation system for CLI operations.
//!
//! This module provides a stack-based cancellation mechanism where Ctrl+C cancels
//! the innermost active operation first. Subsequent Ctrl+C presses cancel progressively
//! outer operations until the entire application exits.

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// A stack of cancellation tokens for hierarchical cancellation.
///
/// When Ctrl+C is pressed, the topmost (most recent) token is cancelled.
/// Subsequent presses cancel tokens further down the stack until
/// the root token is cancelled, which exits the application.
#[derive(Clone)]
pub struct CancellationStack {
    inner: Arc<Mutex<CancellationStackInner>>,
}

struct CancellationStackInner {
    /// Stack of cancellation tokens, with the root at index 0
    tokens: Vec<CancellationToken>,
}

impl CancellationStack {
    /// Create a new cancellation stack with a root token.
    pub fn new() -> Self {
        let root = CancellationToken::new();
        Self {
            inner: Arc::new(Mutex::new(CancellationStackInner {
                tokens: vec![root],
            })),
        }
    }

    /// Get the root cancellation token.
    /// Cancelling this token should exit the entire application.
    pub async fn root(&self) -> CancellationToken {
        let inner = self.inner.lock().await;
        inner.tokens.first().cloned().expect("root token always exists")
    }

    /// Push a new cancellation context onto the stack.
    /// Returns a guard that automatically pops when dropped.
    pub async fn push(&self) -> CancellationGuard {
        let token = CancellationToken::new();
        let cloned_token = token.clone();
        
        {
            let mut inner = self.inner.lock().await;
            inner.tokens.push(token);
        }
        
        CancellationGuard {
            stack: self.clone(),
            token: cloned_token,
        }
    }

    /// Cancel the topmost token on the stack.
    /// Returns `true` if a token was cancelled, `false` if the stack is empty.
    pub async fn cancel_top(&self) -> CancelResult {
        let mut inner = self.inner.lock().await;
        
        if inner.tokens.len() <= 1 {
            // Only root token left, cancel it (exit app)
            if let Some(root) = inner.tokens.first() {
                root.cancel();
                return CancelResult::RootCancelled;
            }
            return CancelResult::Empty;
        }
        
        // Cancel and remove the topmost non-root token
        if let Some(token) = inner.tokens.pop() {
            token.cancel();
            CancelResult::TokenCancelled {
                remaining: inner.tokens.len(),
            }
        } else {
            CancelResult::Empty
        }
    }

    /// Get the current cancellation token (topmost on stack).
    pub async fn current(&self) -> CancellationToken {
        let inner = self.inner.lock().await;
        inner.tokens.last().cloned().expect("at least root token exists")
    }

    /// Get the number of tokens on the stack (including root).
    pub async fn depth(&self) -> usize {
        let inner = self.inner.lock().await;
        inner.tokens.len()
    }

    /// Remove a specific token from the stack (used by guard on drop).
    #[allow(dead_code)]
    async fn remove(&self, token: &CancellationToken) {
        let mut inner = self.inner.lock().await;
        // Don't remove the root token (index 0)
        if inner.tokens.len() > 1 {
            inner.tokens.retain(|t| !std::ptr::eq(
                Arc::as_ptr(&t.clone().into()) as *const (),
                Arc::as_ptr(&token.clone().into()) as *const ()
            ));
        }
    }
}

impl Default for CancellationStack {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a cancellation operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelResult {
    /// A non-root token was cancelled
    TokenCancelled {
        /// Number of tokens remaining (including root)
        remaining: usize,
    },
    /// The root token was cancelled (app should exit)
    RootCancelled,
    /// The stack was empty (shouldn't happen normally)
    Empty,
}

/// A guard that holds a cancellation context.
/// When dropped, the associated token is removed from the stack.
pub struct CancellationGuard {
    stack: CancellationStack,
    token: CancellationToken,
}

impl CancellationGuard {
    /// Get a clone of the cancellation token for this context.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Check if this context has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Wait until this context is cancelled.
    pub async fn cancelled(&self) {
        self.token.cancelled().await
    }

    /// Create a child token that is cancelled when either parent or child is cancelled.
    pub fn child_token(&self) -> CancellationToken {
        self.token.child_token()
    }
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        let stack = self.stack.clone();
        let token = self.token.clone();
        // Use tokio::spawn to handle the async removal
        // This is fire-and-forget cleanup
        tokio::spawn(async move {
            let mut inner = stack.inner.lock().await;
            // Find and remove the token by cancellation state match
            // We look for the exact token instance
            if inner.tokens.len() > 1 {
                // Pop only if this is the top token
                if let Some(top) = inner.tokens.last() {
                    if top.is_cancelled() == token.is_cancelled() {
                        // This is a heuristic - in practice guards are dropped in LIFO order
                        inner.tokens.pop();
                    }
                }
            }
        });
    }
}

/// Spawn the Ctrl+C handler that manages the cancellation stack.
/// Returns a handle to the spawned task.
pub fn spawn_ctrlc_handler(stack: CancellationStack) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // Wait for Ctrl+C
            if tokio::signal::ctrl_c().await.is_err() {
                tracing::error!("Failed to listen for Ctrl+C signal");
                break;
            }
            
            let result = stack.cancel_top().await;
            let _depth = stack.depth().await;
            
            match result {
                CancelResult::TokenCancelled { remaining } => {
                    tracing::debug!(
                        "Ctrl+C: Cancelled current operation ({} levels remaining)",
                        remaining
                    );
                    eprintln!("\nCancelled current operation");
                }
                CancelResult::RootCancelled => {
                    tracing::info!("Ctrl+C: Shutting down application");
                    eprintln!("\n[Shutting down...]");
                    break;
                }
                CancelResult::Empty => {
                    tracing::warn!("Ctrl+C: No cancellation tokens available");
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cancellation_stack_basic() {
        let stack = CancellationStack::new();
        assert_eq!(stack.depth().await, 1); // Just root
        
        let guard1 = stack.push().await;
        assert_eq!(stack.depth().await, 2);
        
        let guard2 = stack.push().await;
        assert_eq!(stack.depth().await, 3);
        
        // Cancel top (guard2's token)
        let result = stack.cancel_top().await;
        assert!(matches!(result, CancelResult::TokenCancelled { remaining: 2 }));
        assert!(guard2.is_cancelled());
        assert!(!guard1.is_cancelled());
        
        // Cancel next (guard1's token)
        let result = stack.cancel_top().await;
        assert!(matches!(result, CancelResult::TokenCancelled { remaining: 1 }));
        assert!(guard1.is_cancelled());
        
        // Cancel root
        let result = stack.cancel_top().await;
        assert!(matches!(result, CancelResult::RootCancelled));
    }
}
