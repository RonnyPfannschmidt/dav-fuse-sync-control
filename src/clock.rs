//! Injectable time source.
//!
//! The cache made five separate `SystemTime::now()` calls, so any test of TTL
//! behaviour had to `sleep`. With this, TTL boundaries are tested exactly and
//! instantly.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Default)]
pub enum Clock {
    #[default]
    System,
    Fixed(Arc<AtomicI64>),
}

impl Clock {
    /// A clock pinned to `secs`, advanceable by tests.
    pub fn fixed(secs: i64) -> Self {
        Clock::Fixed(Arc::new(AtomicI64::new(secs)))
    }

    /// Seconds since the Unix epoch.
    pub fn now_secs(&self) -> i64 {
        match self {
            Clock::System => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                // Only reachable if the system clock is before 1970.
                .unwrap_or(0),
            Clock::Fixed(v) => v.load(Ordering::SeqCst),
        }
    }

    /// Move a fixed clock forward. No-op on the system clock.
    pub fn advance(&self, secs: i64) {
        if let Clock::Fixed(v) = self {
            v.fetch_add(secs, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_advances_only_when_told() {
        let c = Clock::fixed(1_000);
        assert_eq!(c.now_secs(), 1_000);
        assert_eq!(c.now_secs(), 1_000);
        c.advance(30);
        assert_eq!(c.now_secs(), 1_030);
    }

    #[test]
    fn fixed_clock_shares_state_across_clones() {
        let c = Clock::fixed(0);
        let d = c.clone();
        c.advance(5);
        assert_eq!(d.now_secs(), 5);
    }

    #[test]
    fn system_clock_is_after_2020() {
        assert!(Clock::System.now_secs() > 1_577_836_800);
    }
}
