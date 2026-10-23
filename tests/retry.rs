//! Public retry policy behavior.

use chrono::{DateTime, Duration, TimeZone, Utc};
use mpp_autopilot::retry::RetryPolicy;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 19, 5, 0, 0)
        .single()
        .expect("valid timestamp")
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
