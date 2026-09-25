//! Exponential-backoff helpers.

use std::time::Duration;

/// Doubling backoff with a ceiling.
#[derive(Clone, Debug)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    current: Duration,
    jitter: bool,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            current: initial,
            jitter: false,
        }
    }

    /// Add up to ~20% random jitter to each delay, so that many clients
    /// retrying against the same service spread out.
    pub fn with_jitter(mut self) -> Self {
        self.jitter = true;
        self
    }

    /// The delay to wait now; the following call returns twice as much
    /// (capped, plus jitter if enabled).
    pub fn next_delay(&mut self) -> Duration {
        let mut delay = self.current;
        if self.jitter {
            let max_jitter_ms = ((self.current.as_millis() as u64) / 5).clamp(50, 30_000);
            delay += Duration::from_millis(jitter_nanos() % (max_jitter_ms + 1));
        }
        self.current = (self.current * 2).min(self.max);
        delay
    }

    pub fn reset(&mut self) {
        self.current = self.initial;
    }
}

/// Cheap, dependency-free pseudo-randomness; only spreads out retry timing.
fn jitter_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_caps_and_resets() {
        let mut backoff = Backoff::new(Duration::from_secs(5), Duration::from_secs(12));
        assert_eq!(backoff.next_delay(), Duration::from_secs(5));
        assert_eq!(backoff.next_delay(), Duration::from_secs(10));
        assert_eq!(backoff.next_delay(), Duration::from_secs(12));
        assert_eq!(backoff.next_delay(), Duration::from_secs(12));
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_secs(5));
    }

    #[test]
    fn jittered_backoff_stays_within_twenty_percent() {
        let mut b = Backoff::new(Duration::from_secs(2), Duration::from_secs(10)).with_jitter();
        let d1 = b.next_delay();
        assert!(d1 >= Duration::from_secs(2) && d1 <= Duration::from_millis(2_400));
        let d2 = b.next_delay();
        assert!(d2 >= Duration::from_secs(4) && d2 <= Duration::from_millis(4_800));
        for _ in 0..5 {
            let d = b.next_delay();
            assert!(d >= Duration::from_secs(8) && d <= Duration::from_secs(12));
        }
    }
}
