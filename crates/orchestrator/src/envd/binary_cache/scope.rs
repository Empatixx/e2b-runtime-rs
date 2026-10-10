use std::fmt;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeError {
    Cancelled,
    DeadlineExceeded,
}

impl fmt::Display for ScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeError::Cancelled => f.write_str("context canceled"),
            ScopeError::DeadlineExceeded => f.write_str("context deadline exceeded"),
        }
    }
}

#[derive(Clone)]
pub struct Scope {
    cancel: CancellationToken,
    deadline: Option<Instant>,
}

impl Default for Scope {
    fn default() -> Self {
        Self::background()
    }
}

impl Scope {
    pub fn background() -> Self {
        Self {
            cancel: CancellationToken::new(),
            deadline: None,
        }
    }

    pub fn cancellable() -> (Self, CancellationToken) {
        let scope = Self::background();
        let token = scope.cancel.clone();
        (scope, token)
    }

    pub fn child(&self) -> (Self, CancellationToken) {
        let cancel = self.cancel.child_token();
        (
            Self {
                cancel: cancel.clone(),
                deadline: self.deadline,
            },
            cancel,
        )
    }

    pub fn with_timeout(&self, timeout: Duration) -> Self {
        let deadline = Instant::now() + timeout;
        Self {
            cancel: self.cancel.clone(),
            deadline: Some(self.deadline.map_or(deadline, |own| own.min(deadline))),
        }
    }

    pub fn detached(&self) -> Self {
        Self::background()
    }

    pub fn error(&self) -> Option<ScopeError> {
        match (self.cancel.is_cancelled(), self.deadline) {
            (true, _) => Some(ScopeError::Cancelled),
            (false, Some(deadline)) if Instant::now() >= deadline => Some(ScopeError::DeadlineExceeded),
            _ => None,
        }
    }

    pub async fn done(&self) -> ScopeError {
        match self.deadline {
            Some(deadline) => tokio::select! {
                () = self.cancel.cancelled() => ScopeError::Cancelled,
                () = tokio::time::sleep_until(deadline) => ScopeError::DeadlineExceeded,
            },
            None => {
                self.cancel.cancelled().await;
                ScopeError::Cancelled
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_background_scope_never_ends() {
        let scope = Scope::background();

        assert_eq!(scope.error(), None);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), scope.done())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancelling_ends_the_scope_as_cancelled() {
        let (scope, token) = Scope::cancellable();

        token.cancel();

        assert_eq!(scope.error(), Some(ScopeError::Cancelled));
        assert_eq!(scope.done().await, ScopeError::Cancelled);
    }

    #[tokio::test]
    async fn a_timeout_ends_the_scope_as_a_deadline() {
        let scope = Scope::background().with_timeout(Duration::from_millis(10));

        assert_eq!(scope.done().await, ScopeError::DeadlineExceeded);
        assert_eq!(scope.error(), Some(ScopeError::DeadlineExceeded));
    }

    #[tokio::test]
    async fn a_child_deadline_never_extends_its_parent() {
        let parent = Scope::background().with_timeout(Duration::from_millis(10));

        let child = parent.with_timeout(Duration::from_secs(3600));

        assert_eq!(child.done().await, ScopeError::DeadlineExceeded);
    }

    #[tokio::test]
    async fn detaching_drops_cancellation_and_deadline() {
        let (scope, token) = Scope::cancellable();
        let detached = scope.with_timeout(Duration::from_millis(1)).detached();

        token.cancel();
        tokio::time::sleep(Duration::from_millis(5)).await;

        assert_eq!(detached.error(), None);
    }

    #[test]
    fn the_errors_read_like_go_contexts() {
        assert_eq!(ScopeError::Cancelled.to_string(), "context canceled");
        assert_eq!(ScopeError::DeadlineExceeded.to_string(), "context deadline exceeded");
    }
}
