/// Host-provided cancellation signal.
pub trait CancelSignal: Sync {
    /// Returns `true` when the current operation should stop.
    fn is_cancelled(&self) -> bool;
}

impl<F> CancelSignal for F
where
    F: Fn() -> bool + Sync,
{
    fn is_cancelled(&self) -> bool {
        self()
    }
}

/// Cancellation signal that never requests cancellation.
#[derive(Clone, Copy, Debug, Default)]
pub struct NeverCancelled;

impl CancelSignal for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}
