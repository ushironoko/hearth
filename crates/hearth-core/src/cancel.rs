//! Cooperative cancellation shared by every tool.
//!
//! A [`CancelToken`] is a one-way latch. The N-API layer flips it from a JS
//! `AbortSignal`'s `abort` callback (which runs on the JS thread) while the
//! tool runs on a libuv worker thread; the tool polls it at its own safe
//! points. Nothing is preempted — a tool always finishes the step it is in and
//! releases its resources before reporting [`ErrorKind::Cancelled`].
//!
//! The "no token" case is deliberately allocation-free: [`CancelToken::none`]
//! is a `None` inside the struct, so the synchronous, non-cancellable call
//! paths pay nothing for the feature.
//!
//! [`ErrorKind::Cancelled`]: hearth_proto::ErrorKind::Cancelled

use hearth_proto::{ToolError, ToolResult};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cheap-to-clone cancellation latch.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Option<Arc<AtomicBool>>);

impl CancelToken {
    /// A token that can never be cancelled. Allocation-free.
    pub const fn none() -> Self {
        Self(None)
    }

    /// A fresh, not-yet-cancelled token.
    pub fn new() -> Self {
        Self(Some(Arc::new(AtomicBool::new(false))))
    }

    /// Latch the token. Idempotent, and safe from any thread.
    pub fn cancel(&self) {
        if let Some(flag) = &self.0 {
            flag.store(true, Ordering::SeqCst);
        }
    }

    /// Whether cancellation has been requested.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        match &self.0 {
            // `Acquire` pairs with the `SeqCst` store so a worker that observes
            // the latch also observes everything the canceller published first.
            Some(flag) => flag.load(Ordering::Acquire),
            None => false,
        }
    }

    /// Whether this token can ever be cancelled. Lets a hot loop skip its
    /// polling entirely for the non-cancellable case.
    #[inline]
    pub fn is_live(&self) -> bool {
        self.0.is_some()
    }

    /// `Err(Cancelled)` if the token is latched, otherwise `Ok(())`.
    #[inline]
    pub fn check(&self) -> ToolResult<()> {
        if self.is_cancelled() {
            Err(ToolError::cancelled())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_proto::ErrorKind;

    #[test]
    fn none_never_cancels() {
        let t = CancelToken::none();
        assert!(!t.is_live());
        t.cancel();
        assert!(!t.is_cancelled());
        assert!(t.check().is_ok());
    }

    #[test]
    fn cancel_is_visible_across_threads() {
        let t = CancelToken::new();
        let t2 = t.clone();
        let h = std::thread::spawn(move || {
            while !t2.is_cancelled() {
                std::hint::spin_loop();
            }
            t2.check().unwrap_err().kind
        });
        t.cancel();
        assert_eq!(h.join().unwrap(), ErrorKind::Cancelled);
    }
}
