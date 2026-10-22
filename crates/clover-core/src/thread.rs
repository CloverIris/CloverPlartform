//! Small concurrency helpers shared by the surface runtimes.

use std::sync::{Condvar, Mutex, MutexGuard};

/// A single-slot "latest value" cell.
///
/// Producers never block and never queue up: setting a new value drops the old
/// one. That is exactly what a frame hand-off and an input queue want — the
/// newest frame is the only one worth showing, and a stalled consumer should not
/// make the producer allocate without bound.
pub struct Latest<T> {
    slot: Mutex<Option<T>>,
    available: Condvar,
}

impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Latest<T> {
    pub fn new() -> Self {
        Self { slot: Mutex::new(None), available: Condvar::new() }
    }

    fn lock(&self) -> MutexGuard<'_, Option<T>> {
        // A poisoned lock only means a previous holder panicked; the slot itself
        // stays valid, so recovering is correct here.
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replaces the current value, returning the previous one if any.
    pub fn set(&self, value: T) -> Option<T> {
        let previous = {
            let mut slot = self.lock();
            slot.replace(value)
        };
        self.available.notify_one();
        previous
    }

    /// Takes the current value if there is one.
    pub fn take(&self) -> Option<T> {
        self.lock().take()
    }

    pub fn peek_is_some(&self) -> bool {
        self.lock().is_some()
    }

    /// Blocks until a value is available.
    pub fn wait_take(&self) -> T {
        let mut slot = self.lock();
        loop {
            if let Some(value) = slot.take() {
                return value;
            }
            slot = self.available.wait(slot).unwrap_or_else(|e| e.into_inner());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn set_then_take() {
        let latest = Latest::new();
        assert_eq!(latest.take(), None);
        latest.set(7);
        assert_eq!(latest.take(), Some(7));
        assert_eq!(latest.take(), None);
    }

    #[test]
    fn newer_value_replaces_older() {
        let latest = Latest::new();
        assert_eq!(latest.set(1), None);
        assert_eq!(latest.set(2), Some(1));
        assert_eq!(latest.take(), Some(2));
    }

    #[test]
    fn consumer_wakes_up() {
        let latest = Arc::new(Latest::new());
        let consumer = {
            let latest = Arc::clone(&latest);
            thread::spawn(move || latest.wait_take())
        };
        thread::sleep(std::time::Duration::from_millis(20));
        latest.set(42);
        assert_eq!(consumer.join().unwrap(), 42);
    }
}
