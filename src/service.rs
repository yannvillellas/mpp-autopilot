//! Long-running service loop with independent retries and graceful shutdown.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Europe::Paris;
use thiserror::Error;
use tokio::sync::Notify;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::elo::EloSite;
use crate::mpp::MppApi;
use crate::odds::OddsSite;
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
pub async fn run_once<M: MppApi, E: EloSite, O: OddsSite>(
    mode: Mode,
    state: State,
    state_path: PathBuf,
    mpp: &M,
    elo: &E,
    market: &O,
    now: DateTime<Utc>,
) -> Result<RunReport, RunError> {
    let mut orchestrator = Orchestrator::new(mpp, elo, mode, state, state_path).with_market(market);
    orchestrator.execute_once(now).await
}

/// Runs the daemon loop until a scheduling error or a shutdown signal occurs.
///
/// Daily and trigger retries are independent, so a failed daily fetch no
/// longer blocks trigger passes when a previous snapshot is available.
pub async fn run_forever<M: MppApi, E: EloSite, O: OddsSite>(
    mode: Mode,
    daily_hour: u8,
    state: State,
    state_path: PathBuf,
    mpp: &M,
    elo: &E,
    market: &O,
) -> Result<(), ServiceError> {
    let mut orchestrator = Orchestrator::new(mpp, elo, mode, state, state_path).with_market(market);
    let mut schedule = LoopSchedule::new(Utc::now(), daily_hour)?;
    let shutdown = Shutdown::install();

    loop {
        if shutdown.is_requested() {
            info!("shutdown requested");
            break;
        }

        let now = Utc::now();
        if schedule.daily_action(now) {
            match orchestrator.daily_pass(now).await {
                Ok(report) => {
                    schedule.record_daily_success(Utc::now())?;
                    info!(matches = report.matches, "daily pass complete");
                }
                Err(error) => {
                    schedule.record_daily_failure(Utc::now());
                    warn!(
                        error = %error,
                        transient = error.is_transient(),
                        "daily pass failed; retrying independently"
                    );
                }
            }
        }

        if shutdown.is_requested() {
            info!("shutdown requested");
            break;
        }

        let trigger_now = Utc::now();
        if schedule.trigger_action(trigger_now, orchestrator.has_snapshot()) {
            match orchestrator.trigger_pass(trigger_now).await {
                Ok(report) => {
                    if report.triggers > 0 || report.finished > 0 {
                        info!(
                            triggers = report.triggers,
                            writes = report.writes,
                            unchanged = report.unchanged,
                            dry_writes = report.dry_writes,
                            finished = report.finished,
                            skipped = report.skipped,
                            "trigger pass complete"
                        );
                    }
                    schedule.record_trigger_success();
                }
                Err(error) => {
                    schedule.record_trigger_failure(Utc::now());
                    warn!(
                        error = %error,
                        transient = error.is_transient(),
                        "trigger pass failed; retrying independently"
                    );
                }
            }
        }

        if shutdown.is_requested() {
            info!("shutdown requested");
            break;
        }

        let wake_now = Utc::now();
        let wake =
            schedule.next_wake(wake_now, orchestrator.state(), orchestrator.has_snapshot())?;

        tokio::select! {
            _ = sleep_until(wake) => {}
            _ = shutdown.notified() => {
                info!("shutdown requested");
                break;
            }
        }
    }

    info!("service stopped");
    Ok(())
}

struct LoopSchedule {
    daily_hour: u8,
    next_daily_at: DateTime<Utc>,
    last_daily_date: Option<NaiveDate>,
    daily_pending: bool,
    daily_retry: RetryPolicy,
    trigger_retry: RetryPolicy,
}

impl LoopSchedule {
    fn new(now: DateTime<Utc>, daily_hour: u8) -> Result<Self, SchedulerError> {
        Ok(Self {
            daily_hour,
            next_daily_at: scheduler::next_daily(now, daily_hour)?,
            last_daily_date: None,
            daily_pending: true,
            daily_retry: RetryPolicy::default(),
            trigger_retry: RetryPolicy::default(),
        })
    }

    fn daily_action(&self, now: DateTime<Utc>) -> bool {
        if self.daily_pending {
            self.daily_retry.retry_at().is_none_or(|at| now >= at)
        } else {
            scheduler::daily_due(now, self.next_daily_at, self.last_daily_date)
        }
    }

    fn trigger_action(&self, now: DateTime<Utc>, snapshot_loaded: bool) -> bool {
        snapshot_loaded && self.trigger_retry.retry_at().is_none_or(|at| now >= at)
    }

    fn record_daily_success(&mut self, now: DateTime<Utc>) -> Result<(), SchedulerError> {
        self.last_daily_date = Some(now.with_timezone(&Paris).date_naive());
        self.daily_pending = false;
        self.next_daily_at = scheduler::next_daily(now, self.daily_hour)?;
        self.daily_retry.reset();
        Ok(())
    }

    fn record_daily_failure(&mut self, now: DateTime<Utc>) {
        self.daily_pending = true;
        self.daily_retry.record_failure(now);
    }

    fn record_trigger_success(&mut self) {
        self.trigger_retry.reset();
    }

    fn record_trigger_failure(&mut self, now: DateTime<Utc>) {
        self.trigger_retry.record_failure(now);
    }

    fn next_wake(
        &mut self,
        now: DateTime<Utc>,
        state: &State,
        snapshot_loaded: bool,
    ) -> Result<DateTime<Utc>, SchedulerError> {
        if !self.daily_pending && now >= self.next_daily_at {
            self.next_daily_at = scheduler::next_daily(now, self.daily_hour)?;
        }

        let cap = now + MAX_WAKE_INTERVAL;
        let daily_at = if self.daily_pending {
            self.daily_retry.retry_at().unwrap_or(now)
        } else {
            self.next_daily_at
        };

        let trigger_at = if snapshot_loaded {
            match self.trigger_retry.retry_at().filter(|at| *at > now) {
                Some(retry_at) => Some(retry_at),
                None => scheduler::next_trigger_wake(now, &state.matches),
            }
        } else {
            None
        };

        let mut wake = daily_at.min(cap);
        if let Some(trigger_at) = trigger_at {
            wake = wake.min(trigger_at);
        }
        Ok(wake.max(now))
    }
}

struct Shutdown {
    requested: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Shutdown {
    fn install() -> Self {
        let requested = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let flag = Arc::clone(&requested);
        let signal = Arc::clone(&notify);

        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            flag.store(true, Ordering::SeqCst);
            signal.notify_one();
        });

        Self { requested, notify }
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    async fn notified(&self) {
        self.notify.notified().await;
    }
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = ctrl_c_signal() => {}
            }
        }
        Err(error) => {
            warn!("failed to install SIGTERM handler: {error}; waiting for Ctrl-C");
            ctrl_c_signal().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    ctrl_c_signal().await;
}

async fn ctrl_c_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        warn!("failed to install Ctrl-C handler; shutdown signals unavailable");
        std::future::pending::<()>().await;
    }
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

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("valid timestamp")
    }

    fn now() -> DateTime<Utc> {
        utc(2026, 10, 19, 4, 0)
    }

    fn empty_state() -> State {
        State::new(Mode::Dry)
    }

    #[test]
    fn startup_requires_a_daily_fetch_before_triggers() {
        let now = now();
        let mut schedule = LoopSchedule::new(now, 7).expect("schedule");

        assert!(schedule.daily_action(now));
        assert!(!schedule.trigger_action(now, false));

        schedule.record_daily_success(now).expect("daily success");

        assert!(!schedule.daily_action(now));
        assert!(schedule.trigger_action(now, true));
    }

    #[test]
    fn daily_failure_does_not_block_triggers() {
        let now = now();
        let mut schedule = LoopSchedule::new(now, 7).expect("schedule");
        schedule.record_daily_success(now).expect("daily success");
        schedule.record_daily_failure(now);

        assert!(!schedule.daily_action(now));
        assert!(schedule.trigger_action(now, true));
        assert_eq!(
            schedule.next_wake(now, &empty_state(), true).expect("wake"),
            now + Duration::minutes(1)
        );
    }

    #[test]
    fn a_success_before_seven_does_not_refetch_at_seven() {
        let start = now();
        let seven = utc(2026, 10, 19, 5, 0);
        let next_day = utc(2026, 10, 20, 5, 0);
        let mut schedule = LoopSchedule::new(start, 7).expect("schedule");
        schedule.record_daily_success(start).expect("daily success");

        assert!(!schedule.daily_action(seven));
        assert_eq!(
            schedule
                .next_wake(seven, &empty_state(), true)
                .expect("wake"),
            seven + MAX_WAKE_INTERVAL
        );
        assert!(schedule.daily_action(next_day));
    }

    #[test]
    fn trigger_retry_is_independent_from_daily_retry() {
        let now = now();
        let mut schedule = LoopSchedule::new(now, 7).expect("schedule");
        schedule.record_daily_success(now).expect("daily success");
        schedule.record_trigger_failure(now);

        assert!(!schedule.trigger_action(now, true));
        let wake = schedule.next_wake(now, &empty_state(), true).expect("wake");
        assert_eq!(wake, now + Duration::minutes(1));
        assert!(schedule.trigger_action(wake, true));
    }

    #[test]
    fn daily_retry_is_respected() {
        let now = now();
        let mut schedule = LoopSchedule::new(now, 7).expect("schedule");
        schedule.record_daily_failure(now);

        assert!(!schedule.daily_action(now));
        assert!(schedule.daily_action(now + Duration::minutes(1)));
    }

    #[test]
    fn due_trigger_wakes_immediately() {
        let now = now();
        let kickoff = now + Duration::hours(2);
        let mut state = empty_state();
        state
            .matches
            .insert("match-1".to_owned(), MatchState::new(kickoff));
        let mut schedule = LoopSchedule::new(now, 7).expect("schedule");
        schedule.record_daily_success(now).expect("daily success");

        assert_eq!(schedule.next_wake(now, &state, true).expect("wake"), now);
    }
}
