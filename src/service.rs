//! Long-running service loop.

use std::path::PathBuf;

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Europe::Paris;
use thiserror::Error;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::elo::EloSite;
use crate::mpp::MppApi;
use crate::retry::RetryPolicy;
use crate::run::{Orchestrator, RunError, RunReport};
use crate::scheduler::{self, MAX_WAKE_INTERVAL, SchedulerError};
use crate::state::{Mode, State};

/// Errors that stop the service loop itself.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// Scheduling error, normally an invalid daily hour.
    #[error("scheduler error: {0}")]
    Scheduler(#[from] SchedulerError),
}

/// Runs one daily fetch and all due triggers.
pub async fn run_once<M: MppApi, E: EloSite>(
    mode: Mode,
    state: State,
    state_path: PathBuf,
    mpp: &M,
    elo: &E,
    now: DateTime<Utc>,
) -> Result<RunReport, RunError> {
    let mut orchestrator = Orchestrator::new(mpp, elo, mode, state, state_path);
    orchestrator.execute_once(now).await
}

/// Runs the daemon loop until a scheduling error occurs.
///
/// Transient MPP/Elo/state failures are logged and retried with exponential
/// backoff; the loop always performs the startup daily fetch first. Daily
/// fetch retries take precedence over trigger passes, which resume once a
/// daily snapshot succeeds.
pub async fn run_forever<M: MppApi, E: EloSite>(
    mode: Mode,
    daily_hour: u8,
    state: State,
    state_path: PathBuf,
    mpp: &M,
    elo: &E,
) -> Result<(), ServiceError> {
    let mut orchestrator = Orchestrator::new(mpp, elo, mode, state, state_path);
    let mut retry = RetryPolicy::default();
    let mut next_daily_at = scheduler::next_daily(Utc::now(), daily_hour)?;
    let mut last_daily_date: Option<NaiveDate> = None;
    let mut daily_pending = true;

    loop {
        let now = Utc::now();
        let paris_day = now.with_timezone(&Paris).date_naive();

        if daily_pending || scheduler::daily_due(now, next_daily_at, last_daily_date) {
            match orchestrator.daily_pass(now).await {
                Ok(report) => {
                    last_daily_date = Some(paris_day);
                    daily_pending = false;
                    next_daily_at = scheduler::next_daily(now, daily_hour)?;
                    retry.reset();
                    info!(matches = report.matches, "daily pass complete");
                }
                Err(error) => {
                    daily_pending = true;
                    let retry_at = retry.record_failure(now);
                    warn!(
                        error = %error,
                        transient = error.is_transient(),
                        retry_at = %retry_at,
                        "daily pass failed; retrying"
                    );
                    sleep_until(loop_wake(
                        now,
                        next_daily_at,
                        true,
                        retry.retry_at(),
                        orchestrator.state(),
                    ))
                    .await;
                    continue;
                }
            }
        } else if now >= next_daily_at {
            next_daily_at = scheduler::next_daily(now, daily_hour)?;
        }

        if !daily_pending && retry.retry_at().is_none_or(|at| now >= at) {
            match orchestrator.trigger_pass(now).await {
                Ok(report) => {
                    if report.triggers > 0 {
                        info!(
                            triggers = report.triggers,
                            writes = report.writes,
                            unchanged = report.unchanged,
                            dry_writes = report.dry_writes,
                            skipped = report.skipped,
                            "trigger pass complete"
                        );
                    }
                    retry.reset();
                }
                Err(error) => {
                    let retry_at = retry.record_failure(now);
                    warn!(
                        error = %error,
                        transient = error.is_transient(),
                        retry_at = %retry_at,
                        "trigger pass failed; retrying"
                    );
                }
            }
        }

        let wake = loop_wake(
            now,
            next_daily_at,
            daily_pending,
            retry.retry_at(),
            orchestrator.state(),
        );
        sleep_until(wake).await;
    }
}

fn loop_wake(
    now: DateTime<Utc>,
    next_daily_at: DateTime<Utc>,
    daily_pending: bool,
    retry_at: Option<DateTime<Utc>>,
    state: &State,
) -> DateTime<Utc> {
    let cap = now + MAX_WAKE_INTERVAL;

    if daily_pending {
        return retry_at.unwrap_or(now).min(cap).max(now);
    }

    if let Some(retry_at) = retry_at.filter(|retry_at| *retry_at > now) {
        return retry_at.min(next_daily_at).min(cap).max(now);
    }

    scheduler::next_wake(now, next_daily_at, &state.matches)
}

async fn sleep_until(wake: DateTime<Utc>) {
    let duration = (wake - Utc::now()).to_std().unwrap_or_default();
    sleep(duration).await;
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::state::{MatchState, State};

    fn utc(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 19, hour, minute, 0)
            .single()
            .expect("valid timestamp")
    }

    fn empty_state() -> State {
        State::new(Mode::Dry)
    }

    #[test]
    fn pending_daily_waits_for_retry() {
        let now = utc(5, 0);
        let wake = loop_wake(
            now,
            now + Duration::hours(2),
            true,
            Some(now + Duration::minutes(5)),
            &empty_state(),
        );

        assert_eq!(wake, now + Duration::minutes(5));
    }

    #[test]
    fn pending_daily_is_capped_at_one_hour() {
        let now = utc(5, 0);
        let wake = loop_wake(
            now,
            now + Duration::hours(2),
            true,
            Some(now + Duration::hours(2)),
            &empty_state(),
        );

        assert_eq!(wake, now + MAX_WAKE_INTERVAL);
    }

    #[test]
    fn retry_is_bounded_by_the_daily_slot() {
        let now = utc(5, 0);
        let wake = loop_wake(
            now,
            now + Duration::minutes(5),
            false,
            Some(now + Duration::minutes(10)),
            &empty_state(),
        );

        assert_eq!(wake, now + Duration::minutes(5));
    }

    #[test]
    fn retry_precedes_a_later_daily_slot() {
        let now = utc(5, 0);
        let wake = loop_wake(
            now,
            now + Duration::hours(2),
            false,
            Some(now + Duration::minutes(10)),
            &empty_state(),
        );

        assert_eq!(wake, now + Duration::minutes(10));
    }

    #[test]
    fn daily_slot_is_capped_without_retry() {
        let now = utc(5, 0);
        let wake = loop_wake(now, now + Duration::hours(2), false, None, &empty_state());

        assert_eq!(wake, now + MAX_WAKE_INTERVAL);
    }

    #[test]
    fn due_trigger_wakes_immediately() {
        let now = utc(5, 0);
        let kickoff = now + Duration::hours(2);
        let mut state = empty_state();
        state
            .matches
            .insert("match-1".to_owned(), MatchState::new(kickoff));

        let wake = loop_wake(now, now + Duration::hours(2), false, None, &state);

        assert_eq!(wake, now);
    }
}
