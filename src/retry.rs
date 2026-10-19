//! Exponential retry policy for transient failures.

use chrono::{DateTime, Duration, Utc};

/// First retry delay.
pub const BASE_BACKOFF_MINUTES: i64 = 1;

/// Maximum retry delay.
pub const MAX_BACKOFF_MINUTES: i64 = 30;

/// Tracks consecutive failures and the next retry time.
#[derive(Debug, Default, Clone)]
pub struct RetryPolicy {
    failures: u32,
    retry_at: Option<DateTime<Utc>>,
}

impl RetryPolicy {
    /// Returns the next retry time, if a failure is pending.
    pub fn retry_at(&self) -> Option<DateTime<Utc>> {
        self.retry_at
    }

    /// Records a failure and returns the next retry time.
    pub fn record_failure(&mut self, now: DateTime<Utc>) -> DateTime<Utc> {
        self.failures = self.failures.saturating_add(1);
        let retry_at = now + backoff_delay(self.failures);
        self.retry_at = Some(retry_at);
        retry_at
    }

    /// Clears the failure history after a success.
    pub fn reset(&mut self) {
        self.failures = 0;
        self.retry_at = None;
    }
}

fn backoff_delay(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(5);
    let minutes = (BASE_BACKOFF_MINUTES << exponent).min(MAX_BACKOFF_MINUTES);
    Duration::minutes(minutes)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 19, 5, 0, 0)
            .single()
            .expect("valid timestamp")
    }

    #[test]
    fn backoff_doubles_then_caps() {
        assert_eq!(backoff_delay(1), Duration::minutes(1));
        assert_eq!(backoff_delay(2), Duration::minutes(2));
        assert_eq!(backoff_delay(3), Duration::minutes(4));
        assert_eq!(backoff_delay(4), Duration::minutes(8));
        assert_eq!(backoff_delay(5), Duration::minutes(16));
        assert_eq!(backoff_delay(6), Duration::minutes(30));
        assert_eq!(backoff_delay(20), Duration::minutes(30));
    }

    #[test]
    fn failures_advance_retry_at_and_reset() {
        let now = now();
        let mut policy = RetryPolicy::default();

        assert_eq!(policy.retry_at(), None);
        assert_eq!(policy.record_failure(now), now + Duration::minutes(1));
        assert_eq!(policy.retry_at(), Some(now + Duration::minutes(1)));
        assert_eq!(policy.record_failure(now), now + Duration::minutes(2));

        policy.reset();
        assert_eq!(policy.retry_at(), None);
        assert_eq!(policy.record_failure(now), now + Duration::minutes(1));
    }
}
