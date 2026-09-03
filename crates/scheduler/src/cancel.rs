//! Cancellation (amendment section 3.2).
//!
//! Cancellation is normal control flow, not an error (baseline section 39). A
//! cancelled task reports [`crate::TaskState::Cancelled`], never `Failed`.
//!
//! The token is deliberately trivial: a running task polls it in its inner
//! loop, so polling must cost a relaxed atomic load and nothing more.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A shared cancellation flag. Cloning shares the flag; cancelling one clone
/// cancels them all.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Idempotent, and safe to call from any thread.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested.
    ///
    /// `Relaxed` is the right ordering: this is a hint that becomes visible
    /// promptly, and no other memory is published through it.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Number of live handles to this flag; test and diagnostic use.
    pub fn handle_count(&self) -> usize {
        Arc::strong_count(&self.flag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_token_is_not_cancelled() {
        assert!(!CancellationToken::new().is_cancelled());
    }

    #[test]
    fn cancelling_is_visible_through_every_clone() {
        let token = CancellationToken::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancelling_twice_is_harmless() {
        let token = CancellationToken::new();
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancellation_crosses_threads() {
        // The scheduler cancels from the interactive thread while a worker
        // polls; this is that shape, without the scheduler.
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let (started, ready) = std::sync::mpsc::channel();

        let handle = std::thread::spawn(move || {
            started.send(()).expect("the test is still listening");
            while !worker_token.is_cancelled() {
                std::hint::spin_loop();
            }
            // Reaching here is the proof: the loop only exits once the flag
            // set on the other thread became visible on this one.
            true
        });

        ready.recv().expect("the worker started");
        token.cancel();
        assert!(handle.join().expect("worker thread finished"), "the worker saw the cancellation");
    }
}
