//! The clock the auth services read (`DateTime.now`): the system clock in production, a
//! settable one in tests (the TS tests use Effect's `TestClock`).

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// Milliseconds since the Unix epoch.
pub trait Clock: Send + Sync + 'static {
    fn now_millis(&self) -> i64;
}

/// `Date.now()`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> i64 {
        zc_core::now_millis()
    }
}

/// A clock that only moves when told to.
#[derive(Debug, Default)]
pub struct TestClock(AtomicI64);

impl TestClock {
    pub fn new(start_millis: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(start_millis)))
    }

    pub fn set(&self, millis: i64) {
        self.0.store(millis, Ordering::SeqCst);
    }

    pub fn advance(&self, millis: i64) {
        self.0.fetch_add(millis, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_millis(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// The shared clock handle.
pub type SharedClock = Arc<dyn Clock>;

/// The system clock as a [`SharedClock`].
pub fn system_clock() -> SharedClock {
    Arc::new(SystemClock)
}
