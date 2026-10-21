//! Pure scheduling rules for the daily fetch and match triggers.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use mpp_autopilot::scheduler::{
    SchedulerError, daily_due, due_triggers, next_daily, next_trigger_wake, next_wake,
    trigger_due_at,
};
use mpp_autopilot::state::{MatchState, Trigger};

fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .expect("valid timestamp")
}

fn matches_with(kickoff: DateTime<Utc>) -> BTreeMap<String, MatchState> {
    BTreeMap::from([("match-1".to_owned(), MatchState::new(kickoff))])
}

#[test]
fn next_daily_uses_paris_time() {
    let before = utc(2026, 10, 19, 4, 59);
    let at_hour = utc(2026, 10, 19, 5, 0);
    let after = utc(2026, 10, 19, 5, 1);

    assert_eq!(
        next_daily(before, 7).expect("next daily"),
        utc(2026, 10, 19, 5, 0)
    );
    assert_eq!(
        next_daily(at_hour, 7).expect("next daily"),
        utc(2026, 10, 20, 5, 0)
    );
    assert_eq!(
        next_daily(after, 7).expect("next daily"),
        utc(2026, 10, 20, 5, 0)
    );
}

#[test]
fn next_daily_uses_winter_offset() {
    let now = utc(2026, 12, 1, 5, 59);

    assert_eq!(
        next_daily(now, 7).expect("next daily"),
        utc(2026, 12, 1, 6, 0)
    );
}

#[test]
fn next_daily_handles_spring_forward() {
    let now = utc(2026, 3, 29, 0, 30);

    assert_eq!(
        next_daily(now, 7).expect("next daily"),
        utc(2026, 3, 29, 5, 0)
    );
}

#[test]
fn next_daily_handles_fall_back() {
    let now = utc(2026, 10, 25, 0, 30);

    assert_eq!(
        next_daily(now, 7).expect("next daily"),
        utc(2026, 10, 25, 6, 0)
    );
}

#[test]
fn next_daily_resolves_the_spring_gap() {
    let now = utc(2026, 3, 29, 0, 30);

    assert_eq!(
        next_daily(now, 2).expect("next daily"),
        utc(2026, 3, 29, 1, 0)
    );
}

#[test]
fn next_daily_uses_the_second_ambiguous_occurrence() {
    let now = utc(2026, 10, 25, 0, 30);

    assert_eq!(
        next_daily(now, 2).expect("next daily"),
        utc(2026, 10, 25, 1, 0)
    );
}

#[test]
fn next_daily_skips_the_day_after_both_ambiguous_occurrences() {
    let now = utc(2026, 10, 25, 1, 0);

    assert_eq!(
        next_daily(now, 2).expect("next daily"),
        utc(2026, 10, 26, 1, 0)
    );
}

#[test]
fn next_daily_skips_the_day_after_the_spring_gap() {
    let now = utc(2026, 3, 29, 1, 1);

    assert_eq!(
        next_daily(now, 2).expect("next daily"),
        utc(2026, 3, 30, 0, 0)
    );
}

#[test]
fn next_daily_skips_the_spring_gap_at_its_boundary() {
    let now = utc(2026, 3, 29, 1, 0);

    assert_eq!(
        next_daily(now, 2).expect("next daily"),
        utc(2026, 3, 30, 0, 0)
    );
}

#[test]
fn invalid_daily_hour_is_rejected() {
    let now = utc(2026, 10, 19, 4, 59);

    assert!(matches!(
        next_daily(now, 24),
        Err(SchedulerError::InvalidHour(24))
    ));
}

#[test]
fn trigger_times_are_relative_to_kickoff() {
    let kickoff = utc(2026, 10, 23, 18, 45);

    assert_eq!(trigger_due_at(Trigger::Publish, kickoff), None);
    assert_eq!(
        trigger_due_at(Trigger::T60, kickoff),
        Some(utc(2026, 10, 23, 17, 45))
    );
    assert_eq!(
        trigger_due_at(Trigger::T5, kickoff),
        Some(utc(2026, 10, 23, 18, 40))
    );
}

#[test]
fn due_triggers_follow_the_match_timeline() {
    let kickoff = utc(2026, 10, 23, 18, 45);
    let mut state = MatchState::new(kickoff);

    assert_eq!(
        due_triggers(&state, utc(2026, 10, 23, 12, 0)),
        vec![Trigger::Publish]
    );

    state.record_trigger(Trigger::Publish, utc(2026, 10, 23, 12, 0));
    assert!(due_triggers(&state, utc(2026, 10, 23, 17, 44)).is_empty());
    assert_eq!(
        due_triggers(&state, utc(2026, 10, 23, 17, 45)),
        vec![Trigger::T60]
    );

    state.record_trigger(Trigger::T60, utc(2026, 10, 23, 17, 45));
    assert!(due_triggers(&state, utc(2026, 10, 23, 18, 39)).is_empty());
    assert_eq!(
        due_triggers(&state, utc(2026, 10, 23, 18, 40)),
        vec![Trigger::T5]
    );

    state.record_trigger(Trigger::T5, utc(2026, 10, 23, 18, 40));
    assert!(due_triggers(&state, utc(2026, 10, 23, 18, 45)).is_empty());
}

#[test]
fn due_triggers_include_all_pending_after_kickoff() {
    let kickoff = utc(2026, 10, 23, 18, 45);
    let state = MatchState::new(kickoff);

    assert_eq!(
        due_triggers(&state, utc(2026, 10, 24, 12, 0)),
        vec![Trigger::Publish, Trigger::T60, Trigger::T5]
    );
}

#[test]
fn due_triggers_include_all_pending_at_kickoff() {
    let kickoff = utc(2026, 10, 23, 18, 45);
    let state = MatchState::new(kickoff);

    assert_eq!(
        due_triggers(&state, kickoff),
        vec![Trigger::Publish, Trigger::T60, Trigger::T5]
    );
}

#[test]
fn published_trigger_is_never_due_again() {
    let kickoff = utc(2026, 10, 23, 18, 45);
    let mut state = MatchState::new(kickoff);
    state.record_trigger(Trigger::Publish, utc(2026, 10, 23, 12, 0));

    assert!(due_triggers(&state, utc(2026, 10, 23, 12, 1)).is_empty());
}

#[test]
fn next_trigger_wake_prefers_immediate_work() {
    let kickoff = utc(2026, 10, 23, 18, 45);
    let now = utc(2026, 10, 23, 12, 0);
    let matches = matches_with(kickoff);

    assert_eq!(next_trigger_wake(now, &matches), Some(now));
}

#[test]
fn next_trigger_wake_returns_the_next_due_time() {
    let kickoff = utc(2026, 10, 23, 18, 45);
    let now = utc(2026, 10, 23, 17, 0);
    let mut state = MatchState::new(kickoff);
    state.record_trigger(Trigger::Publish, now);
    let matches = BTreeMap::from([("match-1".to_owned(), state)]);

    assert_eq!(
        next_trigger_wake(now, &matches),
        Some(utc(2026, 10, 23, 17, 45))
    );
}

#[test]
fn next_trigger_wake_clamps_overdue_work_to_now() {
    let kickoff = utc(2026, 10, 23, 18, 45);
    let now = utc(2026, 10, 23, 18, 0);
    let mut state = MatchState::new(kickoff);
    state.record_trigger(Trigger::Publish, utc(2026, 10, 23, 12, 0));
    let matches = BTreeMap::from([("match-1".to_owned(), state)]);

    assert_eq!(next_trigger_wake(now, &matches), Some(now));
}

#[test]
fn next_trigger_wake_returns_none_when_everything_ran() {
    let kickoff = utc(2026, 10, 23, 18, 45);
    let mut state = MatchState::new(kickoff);
    state.record_trigger(Trigger::Publish, utc(2026, 10, 23, 12, 0));
    state.record_trigger(Trigger::T60, utc(2026, 10, 23, 17, 45));
    state.record_trigger(Trigger::T5, utc(2026, 10, 23, 18, 40));
    let matches = BTreeMap::from([("match-1".to_owned(), state)]);

    assert_eq!(next_trigger_wake(utc(2026, 10, 23, 18, 45), &matches), None);
}

#[test]
fn next_wake_is_capped_at_one_hour() {
    let now = utc(2026, 10, 19, 4, 0);
    let daily = utc(2026, 10, 19, 6, 0);

    assert_eq!(
        next_wake(now, daily, &BTreeMap::new()),
        utc(2026, 10, 19, 5, 0)
    );
}

#[test]
fn next_wake_uses_the_earliest_event() {
    let now = utc(2026, 10, 23, 17, 0);
    let kickoff = utc(2026, 10, 23, 18, 45);
    let mut state = MatchState::new(kickoff);
    state.record_trigger(Trigger::Publish, now);
    let matches = BTreeMap::from([("match-1".to_owned(), state)]);

    assert_eq!(
        next_wake(now, utc(2026, 10, 24, 5, 0), &matches),
        utc(2026, 10, 23, 17, 45)
    );
    assert_eq!(
        next_wake(now, utc(2026, 10, 23, 17, 30), &matches),
        utc(2026, 10, 23, 17, 30)
    );
    assert_eq!(
        next_wake(now, utc(2026, 10, 23, 17, 30), &BTreeMap::new()),
        utc(2026, 10, 23, 17, 30)
    );
}

#[test]
fn daily_fetch_is_blocked_after_the_first_ambiguous_occurrence() {
    let first = utc(2026, 10, 25, 0, 0);
    let second = utc(2026, 10, 25, 1, 0);
    let paris_day = NaiveDate::from_ymd_opt(2026, 10, 25).expect("valid date");
    let next_day = utc(2026, 10, 26, 1, 0);

    assert!(daily_due(first, first, None));
    assert!(!daily_due(second, second, Some(paris_day)));
    assert!(daily_due(second, second, None));
    assert!(daily_due(next_day, next_day, Some(paris_day)));
}

#[test]
fn daily_fetch_is_not_due_before_its_slot() {
    let now = utc(2026, 10, 19, 4, 59);
    let scheduled = utc(2026, 10, 19, 5, 0);
    let paris_day = NaiveDate::from_ymd_opt(2026, 10, 19).expect("valid date");

    assert!(!daily_due(now, scheduled, None));
    assert!(!daily_due(scheduled, scheduled, Some(paris_day)));
}

#[test]
fn an_already_fetched_slot_advances_to_the_next_day() {
    let second = utc(2026, 10, 25, 1, 0);
    let paris_day = NaiveDate::from_ymd_opt(2026, 10, 25).expect("valid date");

    assert!(!daily_due(second, second, Some(paris_day)));
    assert_eq!(
        next_daily(second, 2).expect("next daily"),
        utc(2026, 10, 26, 1, 0)
    );
}
