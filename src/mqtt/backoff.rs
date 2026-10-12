//! Delays between retries: the broker connection and the per-camera MQTT listeners

use tokio::time::Duration;

/// Doubling delays from `min` to `max`, back to `min` after [`Backoff::reset`]
pub(crate) struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub(crate) fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max,
            next: min,
        }
    }

    /// The delay before the next retry
    pub(crate) fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(self.max);
        delay
    }

    pub(crate) fn reset(&mut self) {
        self.next = self.min;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_doubles_to_max_and_resets() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(60));
        let delays: Vec<u64> = (0..9).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }
}
