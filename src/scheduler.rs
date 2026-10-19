//! Pure scheduling rules for the daily fetch and match triggers.
//!
//! Every function receives the current time, so the scheduler can be tested
//! without a real clock.

use std::collections::BTreeMap;

use chrono::{DateTime, Days, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Europe::Paris;
use thiserror::Error;

use crate::state::{MatchState, Trigger};

/// Maximum sleep between two scheduler checks.
pub const MAX_WAKE_INTERVAL: Duration = Duration::hours(1);

/// Errors raised by the scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SchedulerError {
    /// The configured daily hour is not between 0 and 23.
    #[error("invalid daily hour {0}, expected 0..=23")]
    InvalidHour(u8),
    /// No daily slot could be resolved, which should not happen in Paris.
    #[error("could not find the next daily slot")]
    NoDailySlot,
}

/// Returns the next occurrence of `hour:00` in Europe/Paris, strictly after
/// `now`.
///
/// A daylight-saving gap is shifted forward to the first valid local time.
/// Ambiguous times resolve to the occurrence that comes next after `now`.
pub fn next_daily(now: DateTime<Utc>, hour: u8) -> Result<DateTime<Utc>, SchedulerError> {
    if hour > 23 {
        return Err(SchedulerError::InvalidHour(hour));
    }

    let local_now = now.with_timezone(&Paris);
    let today = local_now.date_naive();

    for offset in 0..=2 {
        let Some(date) = today.checked_add_days(Days::new(offset)) else {
            continue;
        };
        let Some(naive) = date.and_hms_opt(u32::from(hour), 0, 0) else {
            continue;
        };

        if let Some(candidate) = resolve_candidates(naive)
            .into_iter()
            .filter(|candidate| *candidate > now)
            .min()
        {
            return Ok(candidate);
        }
    }

    Err(SchedulerError::NoDailySlot)
}

/// Returns whether a successful daily fetch is due, allowing at most one
/// successful fetch per Europe/Paris calendar day. Failed fetches may be
/// retried.
///
/// `scheduled` is the slot returned by [`next_daily`] and `last_fetch_date`
/// is the Paris date of the last successful fetch. A missing date means no
/// fetch has succeeded yet, so a retry is allowed.
///
/// When `now >= scheduled` but the Paris date was already fetched, the caller
/// must advance `scheduled` with [`next_daily`] before passing it to
/// [`next_wake`]. After a failed fetch, `scheduled` may be in the past: use
/// the orchestrator's retry/backoff wake instead of passing it to
/// [`next_wake`].
pub fn daily_due(
    now: DateTime<Utc>,
    scheduled: DateTime<Utc>,
    last_fetch_date: Option<NaiveDate>,
) -> bool {
    now >= scheduled && last_fetch_date != Some(now.with_timezone(&Paris).date_naive())
}

/// Returns the scheduled time of a trigger, or [`None`] for `publish`.
///
/// `publish` runs on the first daily pass where the match is seen, so it has
/// no time relative to kickoff.
pub fn trigger_due_at(trigger: Trigger, kickoff: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match trigger {
        Trigger::Publish => None,
        Trigger::T60 => Some(kickoff - Duration::minutes(60)),
        Trigger::T5 => Some(kickoff - Duration::minutes(5)),
    }
}

/// Returns the triggers that are due and have not run yet.
///
/// `publish` is due immediately. After kickoff, every pending trigger is
/// returned so the orchestrator can mark it as executed without writing.
pub fn due_triggers(match_state: &MatchState, now: DateTime<Utc>) -> Vec<Trigger> {
    Trigger::ALL
        .into_iter()
        .filter(|trigger| !match_state.is_trigger_done(*trigger))
        .filter(|trigger| {
            trigger_due_at(*trigger, match_state.kickoff).is_none_or(|due| now >= due)
        })
        .collect()
}

/// Returns the next time a pending trigger should run, clamped to `now` when
/// it is already overdue.
pub fn next_trigger_wake(
    now: DateTime<Utc>,
    matches: &BTreeMap<String, MatchState>,
) -> Option<DateTime<Utc>> {
    let mut next: Option<DateTime<Utc>> = None;

    for state in matches.values() {
        for trigger in Trigger::ALL {
            if state.is_trigger_done(trigger) {
                continue;
            }

            let due = trigger_due_at(trigger, state.kickoff).map_or(now, |due| due.max(now));
            next = Some(next.map_or(due, |current| current.min(due)));
        }
    }

    next
}

/// Returns the next wake-up, capped at [`MAX_WAKE_INTERVAL`] and never earlier
/// than `now`.
///
/// `next_daily` must be strictly after `now`. When a wake-up is late, the
/// caller must handle the daily fetch and recompute the next slot before
/// calling this function again.
pub fn next_wake(
    now: DateTime<Utc>,
    next_daily: DateTime<Utc>,
    matches: &BTreeMap<String, MatchState>,
) -> DateTime<Utc> {
    debug_assert!(
        next_daily > now,
        "next_daily must be strictly in the future"
    );

    let cap = now + MAX_WAKE_INTERVAL;
    let mut wake = next_daily.min(cap);

    if let Some(trigger) = next_trigger_wake(now, matches) {
        wake = wake.min(trigger);
    }

    wake.max(now)
}

fn resolve_candidates(naive: NaiveDateTime) -> Vec<DateTime<Utc>> {
    match Paris.from_local_datetime(&naive) {
        LocalResult::Single(local) => vec![local.with_timezone(&Utc)],
        LocalResult::Ambiguous(first, second) => {
            vec![first.with_timezone(&Utc), second.with_timezone(&Utc)]
        }
        LocalResult::None => {
            let shifted = naive + Duration::hours(1);
            match Paris.from_local_datetime(&shifted) {
                LocalResult::Single(local) => vec![local.with_timezone(&Utc)],
                LocalResult::Ambiguous(first, second) => {
                    vec![first.with_timezone(&Utc), second.with_timezone(&Utc)]
                }
                LocalResult::None => Vec::new(),
            }
        }
    }
}
