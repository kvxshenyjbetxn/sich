//! Reconnect backoff and liveness timing. Pure, no I/O.

use std::time::Duration;

/// Delays between reconnect attempts; the last one repeats forever.
pub const SCHEDULE_MS: [u64; 8] = [500, 1_000, 2_000, 4_000, 8_000, 15_000, 30_000, 60_000];

/// App-level ping period.
pub const PING_INTERVAL: Duration = Duration::from_secs(5);
/// A connection is dead after this many ping periods without any message from the gateway.
pub const MISSED_PONGS: u32 = 3;

#[derive(Debug, Default, Clone)]
pub struct Backoff {
    attempt: usize,
}

impl Backoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of delays handed out since the last reset (1-based attempt number of the next retry).
    pub fn attempt(&self) -> usize {
        self.attempt
    }

    /// The delay before the next retry, advancing the schedule.
    pub fn next_delay(&mut self) -> Duration {
        let i = self.attempt.min(SCHEDULE_MS.len() - 1);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(SCHEDULE_MS[i])
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// True when nothing was received for `MISSED_PONGS` ping periods.
pub fn is_dead(since_last_rx: Duration) -> bool {
    since_last_rx >= PING_INTERVAL * MISSED_PONGS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_schedule_then_repeats_last() {
        let mut b = Backoff::new();
        let got: Vec<u64> = (0..11).map(|_| b.next_delay().as_millis() as u64).collect();
        assert_eq!(got, vec![500, 1000, 2000, 4000, 8000, 15000, 30000, 60000, 60000, 60000, 60000]);
        assert_eq!(b.attempt(), 11);
    }

    #[test]
    fn reset_restarts_schedule() {
        let mut b = Backoff::new();
        for _ in 0..5 {
            b.next_delay();
        }
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.next_delay(), Duration::from_millis(500));
    }

    #[test]
    fn does_not_overflow_after_many_attempts() {
        let mut b = Backoff::new();
        for _ in 0..10_000 {
            b.next_delay();
        }
        assert_eq!(b.next_delay(), Duration::from_secs(60));
    }

    #[test]
    fn liveness_after_three_missed_pings() {
        assert!(!is_dead(Duration::from_secs(14)));
        assert!(is_dead(Duration::from_secs(15)));
        assert!(is_dead(Duration::from_secs(60)));
    }
}
