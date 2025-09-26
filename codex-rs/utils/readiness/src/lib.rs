//! Readiness flag with token-based authorization and async waiting (Tokio).

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio::time;

/// Opaque subscription token returned by `subscribe()`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Token(i32);

const LOCK_TIMEOUT: Duration = Duration::from_millis(1000);

#[async_trait::async_trait]
pub trait Readiness: Send + Sync + 'static {
    /// Returns true if the flag is currently marked ready. At least one token needs to be marked
    /// as ready before.
    /// `true` is not reversible.
    fn is_ready(&self) -> bool;

    /// Subscribe to readiness and receive an authorization token.
    ///
    /// If the flag is already ready, returns `FlagAlreadyReady`.
    async fn subscribe(&self) -> Result<Token, errors::ReadinessError>;

    /// Attempt to mark the flag ready, validated by the provided token.
    ///
    /// Returns `true` iff:
    /// - `token` is currently subscribed, and
    /// - the flag was not already ready.
    async fn mark_ready(&self, token: Token) -> Result<bool, errors::ReadinessError>;

    /// Asynchronously wait until the flag becomes ready.
    async fn wait_ready(&self);
}

pub struct ReadinessFlag {
    /// Atomic for cheap reads.
    ready: AtomicBool,
    /// Used to generate the next i32 token.
    next_id: AtomicI32,
    /// Set of active subscriptions.
    tokens: Mutex<HashSet<Token>>,
    /// Broadcasts readiness to async waiters.
    tx: watch::Sender<bool>,
}

impl ReadinessFlag {
    /// Create a new, not-yet-ready flag.
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self {
            ready: AtomicBool::new(false),
            next_id: AtomicI32::new(1), // Reserve 0.
            tokens: Mutex::new(HashSet::new()),
            tx,
        }
    }

    async fn with_tokens<R>(
        &self,
        f: impl FnOnce(&mut HashSet<Token>) -> R,
    ) -> Result<R, errors::ReadinessError> {
        let mut guard = time::timeout(LOCK_TIMEOUT, self.tokens.lock())
            .await
            .map_err(|_| errors::ReadinessError::TokenLockFailed)?;
        Ok(f(&mut guard))
    }
}

impl Default for ReadinessFlag {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Readiness for ReadinessFlag {
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    async fn subscribe(&self) -> Result<Token, errors::ReadinessError> {
        if self.is_ready() {
            return Err(errors::ReadinessError::FlagAlreadyReady);
        }

        // Generate a token; ensure it's not 0.
        let token = Token(self.next_id.fetch_add(1, Ordering::Relaxed));

        // Recheck readiness while holding the lock so mark_ready can't flip the flag between the
        // check above and inserting the token.
        let inserted = self
            .with_tokens(|tokens| {
                if self.is_ready() {
                    return false;
                }
                tokens.insert(token);
                true
            })
            .await?;

        if !inserted {
            return Err(errors::ReadinessError::FlagAlreadyReady);
        }

        Ok(token)
    }

    async fn mark_ready(&self, token: Token) -> Result<bool, errors::ReadinessError> {
        if self.is_ready() {
            return Ok(false);
        }
        if token.0 == 0 {
            return Ok(false); // Never authorize.
        }

        let marked = self
            .with_tokens(|set| {
                if !set.remove(&token) {
                    return false; // invalid or already used
                }
                self.ready.store(true, Ordering::Release);
                set.clear(); // no further tokens needed once ready
                true
            })
            .await?;
        if !marked {
            return Ok(false);
        }
        // Best-effort broadcast; ignore error if there are no receivers.
        let _ = self.tx.send(true);
        Ok(true)
    }

    async fn wait_ready(&self) {
        if self.is_ready() {
            return;
        }
        let mut rx = self.tx.subscribe();
        // Fast-path check before awaiting.
        if *rx.borrow() {
            return;
        }
        // Await changes until true is observed.
        while rx.changed().await.is_ok() {
            if *rx.borrow() {
                break;
            }
        }
    }
}

mod errors {
    use thiserror::Error;

    #[derive(Debug, Error)]
    pub enum ReadinessError {
        #[error("Failed to acquire readiness token lock")]
        TokenLockFailed,
        #[error("Flag is already ready. Impossible to subscribe")]
        FlagAlreadyReady,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::Readiness;
    use super::ReadinessFlag;
    use super::Token;
    use super::errors::ReadinessError;

    #[tokio::test]
    async fn subscribe_and_mark_ready_roundtrip() -> Result<(), ReadinessError> {
        let flag = ReadinessFlag::new();
        let token = flag.subscribe().await?;

        assert!(flag.mark_ready(token).await?);
        assert!(flag.is_ready());
        Ok(())
    }

    #[tokio::test]
    async fn subscribe_after_ready_returns_none() -> Result<(), ReadinessError> {
        let flag = ReadinessFlag::new();
        let token = flag.subscribe().await?;
        assert!(flag.mark_ready(token).await?);

        assert!(flag.subscribe().await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn mark_ready_rejects_unknown_token() -> Result<(), ReadinessError> {
        let flag = ReadinessFlag::new();
        assert!(!flag.mark_ready(Token(42)).await?);
        assert!(!flag.is_ready());
        Ok(())
    }

    #[tokio::test]
    async fn wait_ready_unblocks_after_mark_ready() -> Result<(), ReadinessError> {
        let flag = Arc::new(ReadinessFlag::new());
        let token = flag.subscribe().await?;

        let waiter = {
            let flag = Arc::clone(&flag);
            tokio::spawn(async move {
                flag.wait_ready().await;
            })
        };

        assert!(flag.mark_ready(token).await?);
        waiter.await.expect("waiting task should not panic");
        Ok(())
    }

    #[tokio::test]
    async fn mark_ready_twice_uses_single_token() -> Result<(), ReadinessError> {
        let flag = ReadinessFlag::new();
        let token = flag.subscribe().await?;

        assert!(flag.mark_ready(token).await?);
        assert!(!flag.mark_ready(token).await?);
        Ok(())
    }

    #[tokio::test]
    async fn subscribe_returns_error_when_lock_is_held() {
        let flag = ReadinessFlag::new();
        let _guard = flag
            .tokens
            .try_lock()
            .expect("initial lock acquisition should succeed");

        let err = flag
            .subscribe()
            .await
            .expect_err("contended subscribe should report a lock failure");
        assert!(matches!(err, ReadinessError::TokenLockFailed));
    }
}
