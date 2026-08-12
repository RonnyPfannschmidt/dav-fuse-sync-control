//! Client-side rate limiting for Drive.
//!
//! The published quotas are not the binding constraint. A 60-second delta poll
//! costs about 144k units a day against a 400M/day allowance, and a 50,000-file
//! enumeration costs 5,000 -- both noise. What actually bites is an
//! undocumented per-second throttle that answers `403 userRateLimitExceeded`.
//!
//! The defaults match rclone's empirically-derived ones
//! (`--drive-pacer-min-sleep 100ms --drive-pacer-burst 100`): roughly 10
//! requests per second sustained, with a burst of 100. Pacing here is cheaper
//! than discovering the limit through 403s and backing off after the fact.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A token bucket.
#[derive(Debug)]
pub struct Pacer {
    inner: Mutex<Bucket>,
    /// Minimum spacing between requests once the burst is spent.
    min_sleep: Duration,
    burst: u32,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Default for Pacer {
    fn default() -> Self {
        Self::new(Duration::from_millis(100), 100)
    }
}

impl Pacer {
    pub fn new(min_sleep: Duration, burst: u32) -> Self {
        Self {
            inner: Mutex::new(Bucket {
                tokens: burst as f64,
                last: Instant::now(),
            }),
            min_sleep,
            burst,
        }
    }

    /// A pacer that never delays, for tests.
    pub fn unlimited() -> Self {
        Self::new(Duration::ZERO, u32::MAX)
    }

    /// Requests per second once the burst is exhausted.
    fn rate(&self) -> f64 {
        if self.min_sleep.is_zero() {
            f64::INFINITY
        } else {
            1.0 / self.min_sleep.as_secs_f64()
        }
    }

    /// How long the caller must wait before its request may go out.
    ///
    /// Split from [`Self::acquire`] so the arithmetic is testable without
    /// actually sleeping.
    pub fn reserve(&self) -> Duration {
        if self.min_sleep.is_zero() {
            return Duration::ZERO;
        }

        let mut bucket = self.inner.lock().unwrap();
        let now = Instant::now();

        // Refill for the time that has passed.
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rate()).min(self.burst as f64);
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Duration::ZERO;
        }

        // Wait for the next token, and take it.
        let deficit = 1.0 - bucket.tokens;
        bucket.tokens = 0.0;
        Duration::from_secs_f64(deficit / self.rate())
    }

    /// Wait until a request may go out.
    pub async fn acquire(&self) {
        let wait = self.reserve();
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_passes_without_delay() {
        let p = Pacer::new(Duration::from_millis(100), 10);
        for i in 0..10 {
            assert_eq!(p.reserve(), Duration::ZERO, "request {i} was delayed");
        }
    }

    #[test]
    fn requests_past_the_burst_are_spaced_out() {
        let p = Pacer::new(Duration::from_millis(100), 3);
        for _ in 0..3 {
            assert_eq!(p.reserve(), Duration::ZERO);
        }
        // The bucket is empty; the next one must wait for a refill.
        let wait = p.reserve();
        assert!(wait > Duration::ZERO, "no throttling after the burst");
        assert!(
            wait <= Duration::from_millis(100),
            "waited longer than one interval: {wait:?}"
        );
    }

    #[test]
    fn the_bucket_refills_over_time() {
        let p = Pacer::new(Duration::from_millis(10), 2);
        p.reserve();
        p.reserve();
        assert!(p.reserve() > Duration::ZERO);

        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            p.reserve(),
            Duration::ZERO,
            "the bucket did not refill while idle"
        );
    }

    #[test]
    fn the_bucket_does_not_overfill_while_idle() {
        let p = Pacer::new(Duration::from_millis(1), 3);
        std::thread::sleep(Duration::from_millis(50)); // would be 50 tokens

        for _ in 0..3 {
            assert_eq!(p.reserve(), Duration::ZERO);
        }
        assert!(
            p.reserve() > Duration::ZERO,
            "the burst cap was exceeded, so a long idle would allow a flood"
        );
    }

    #[test]
    fn an_unlimited_pacer_never_waits() {
        let p = Pacer::unlimited();
        for _ in 0..1000 {
            assert_eq!(p.reserve(), Duration::ZERO);
        }
    }

    #[test]
    fn the_default_matches_rclones_empirical_settings() {
        let p = Pacer::default();
        assert_eq!(p.burst, 100);
        assert_eq!(p.min_sleep, Duration::from_millis(100));
        assert!((p.rate() - 10.0).abs() < f64::EPSILON, "expected ~10 rps");
    }

    #[tokio::test]
    async fn acquire_actually_delays() {
        let p = Pacer::new(Duration::from_millis(20), 1);
        p.acquire().await; // consumes the burst

        let start = Instant::now();
        p.acquire().await;
        assert!(
            start.elapsed() >= Duration::from_millis(10),
            "acquire returned too quickly: {:?}",
            start.elapsed()
        );
    }
}
