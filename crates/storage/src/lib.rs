#![allow(clippy::result_large_err)]
pub(crate) use kloudlite_core::{err, hex, Error, Result};
pub mod auth;
pub mod cache;
pub mod config;
pub mod events;
pub mod index;
pub mod metered;
pub mod ownership;
pub mod pool;
pub mod refmeta;
pub mod store;

/// Take a `std::sync::Mutex` without dying of someone else's panic.
///
/// Every mutex in this crate guards a plain map or counter — the pool's entry table, the cache's
/// in-memory fallback, `keyed_locks`, the pending pull counts. A panic while one is held poisons
/// it, and `.lock().unwrap()` then turns that single panic into a process that panics on EVERY
/// subsequent request touching that map: one bad request wedged a node until it was rolled
/// (2026-09-12). None of these guard an invariant a half-written update could break — the worst a
/// recovered guard sees is a map missing one entry — so recovering is strictly better than
/// propagating the poison. Logged once per process: a poisoned mutex stays poisoned, so warning
/// per acquisition would be the storm, not the signal.
pub trait LockOrRecover<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockOrRecover<T> for std::sync::Mutex<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|p| {
            static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::error!("mutex.poisoned");
            }
            p.into_inner()
        })
    }
}

#[cfg(test)]
mod lock_tests {
    use super::LockOrRecover;

    /// A panic under the lock must not turn into a panic on every later acquisition.
    #[test]
    fn a_poisoned_mutex_still_hands_back_its_value() {
        let m = std::sync::Arc::new(std::sync::Mutex::new(1u32));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("boom");
        })
        .join();
        assert!(m.lock().is_err(), "the mutex really is poisoned");
        assert_eq!(*m.lock_or_recover(), 1);
    }
}
