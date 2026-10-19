//! Orchestrator behavior with fake MPP and ClubElo clients.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Duration, TimeZone, Utc};
use mpp::elo::{EloError, EloRatings, EloSite};
use mpp::model::{Odds, Score};
use mpp::mpp::{Club, Fixture, MppApi, MppError, Snapshot};
use mpp::run::{Orchestrator, RunError};
use mpp::state::{MatchState, Mode, State, Trigger};
use reqwest::StatusCode;

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mpp-run-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct FakeMpp {
    snapshot: Snapshot,
    writes: Mutex<Vec<(String, Score)>>,
    fail_status: Mutex<Option<StatusCode>>,
}

impl FakeMpp {
    fn new(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            writes: Mutex::new(Vec::new()),
            fail_status: Mutex::new(None),
        }
    }

    fn fail_writes(&self, status: StatusCode) {
        *self.fail_status.lock().expect("fail status lock") = Some(status);
    }

    fn writes(&self) -> Vec<(String, Score)> {
        self.writes.lock().expect("writes lock").clone()
    }
}

impl MppApi for FakeMpp {
    async fn fetch_snapshot(&self) -> Result<Snapshot, MppError> {
        Ok(self.snapshot.clone())
    }

    async fn submit_forecast(&self, match_id: &str, score: Score) -> Result<(), MppError> {
        if let Some(status) = *self.fail_status.lock().expect("fail status lock") {
            return Err(MppError::Status {
                status,
                body: "forced write failure".to_owned(),
            });
        }

        self.writes
            .lock()
            .expect("writes lock")
            .push((match_id.to_owned(), score));
        Ok(())
    }
}

struct FakeElo {
    ratings: EloRatings,
    calls: AtomicUsize,
}

impl FakeElo {
    fn new(ratings: EloRatings) -> Self {
        Self {
            ratings,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl EloSite for FakeElo {
    async fn fetch_ratings(&self) -> Result<EloRatings, EloError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(self.ratings.clone())
    }
}

fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .expect("valid timestamp")
}

fn now() -> DateTime<Utc> {
    utc(2026, 10, 19, 5, 0)
}

fn kickoff() -> DateTime<Utc> {
    utc(2026, 10, 23, 18, 45)
}

fn odds() -> Odds {
    Odds {
        home: 81.0,
        draw: 117.0,
        away: 112.0,
    }
}

fn fixture(kickoff: DateTime<Utc>) -> Fixture {
    Fixture {
        id: "match-1".to_owned(),
        game_week: Some(8),
        kickoff,
        odds: odds(),
        home: Club {
            id: "club_psg".to_owned(),
            name: "Paris SG".to_owned(),
        },
        away: Club {
            id: "club_om".to_owned(),
            name: "Marseille".to_owned(),
        },
    }
}

fn snapshot(kickoff: DateTime<Utc>) -> Snapshot {
    Snapshot {
        clubs: HashMap::new(),
        matches: vec![fixture(kickoff)],
    }
}

fn ratings() -> EloRatings {
    EloRatings::from_pairs([
        ("Paris SG", 1684.0983240112262),
        ("Marseille", 1670.9520303480226),
    ])
}

fn state_with_match(mode: Mode, kickoff: DateTime<Utc>) -> State {
    let mut state = State::new(mode);
    state
        .matches
        .insert("match-1".to_owned(), MatchState::new(kickoff));
    state
}

#[tokio::test]
async fn write_mode_writes_publish_and_persists() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path.clone(),
    );

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.matches, 1);
    assert_eq!(report.triggers, 1);
    assert_eq!(report.writes, 1);
    assert_eq!(report.dry_writes, 0);
    assert_eq!(report.unchanged, 0);
    assert_eq!(report.skipped, 0);
    assert_eq!(
        mpp.writes(),
        vec![("match-1".to_owned(), Score { home: 1, away: 1 })]
    );

    let saved = &runner.state().matches["match-1"];
    assert!(saved.is_trigger_done(Trigger::Publish));
    assert_eq!(saved.written_score(), Some("1-1"));
    assert_eq!(saved.written_at, Some(now()));
    assert_eq!(elo.calls(), 1);

    let loaded = State::load(&path, Mode::Write).expect("load state");
    assert_eq!(loaded.matches["match-1"].written_score(), Some("1-1"));
    assert!(loaded.matches["match-1"].is_trigger_done(Trigger::Publish));
}

#[tokio::test]
async fn dry_run_marks_the_trigger_without_writing() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Dry,
        state_with_match(Mode::Dry, kickoff()),
        path,
    );

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.triggers, 1);
    assert_eq!(report.writes, 0);
    assert_eq!(report.dry_writes, 1);
    assert!(mpp.writes().is_empty());

    let saved = &runner.state().matches["match-1"];
    assert!(saved.is_trigger_done(Trigger::Publish));
    assert_eq!(saved.written_score(), None);
    assert_eq!(saved.written_at, None);
}

#[tokio::test]
async fn unchanged_score_is_not_rewritten() {
    let mut state = state_with_match(Mode::Write, kickoff());
    {
        let saved = state.matches.get_mut("match-1").expect("match state");
        saved.record_trigger(Trigger::Publish, now());
        saved.record_write(Score { home: 1, away: 1 }, now());
    }

    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path);

    let report = runner
        .execute_once(kickoff() - Duration::minutes(60))
        .await
        .expect("execute once");

    assert_eq!(report.triggers, 1);
    assert_eq!(report.writes, 0);
    assert_eq!(report.unchanged, 1);
    assert!(mpp.writes().is_empty());
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::T60));
}

#[tokio::test]
async fn failed_write_does_not_mark_the_trigger() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    mpp.fail_writes(StatusCode::INTERNAL_SERVER_ERROR);
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path,
    );

    let error = runner.execute_once(now()).await.expect_err("write failure");

    assert!(matches!(error, RunError::Mpp(_)));
    let saved = &runner.state().matches["match-1"];
    assert!(!saved.is_trigger_done(Trigger::Publish));
    assert_eq!(saved.written_score(), None);
}

#[tokio::test]
async fn missing_elo_skips_and_marks_the_trigger() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(EloRatings::from_pairs([("Paris SG", 1684.0)]));
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path,
    );

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.triggers, 1);
    assert_eq!(report.writes, 0);
    assert_eq!(report.skipped, 1);
    assert!(mpp.writes().is_empty());
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
}

#[tokio::test]
async fn past_kickoff_marks_pending_triggers_without_elo() {
    let past_kickoff = now() - Duration::hours(2);
    let mpp = FakeMpp::new(snapshot(past_kickoff));
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, past_kickoff),
        path,
    );

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.triggers, 3);
    assert_eq!(report.skipped, 3);
    assert_eq!(report.writes, 0);
    assert_eq!(elo.calls(), 0);
    assert!(mpp.writes().is_empty());

    let saved = &runner.state().matches["match-1"];
    for trigger in Trigger::ALL {
        assert!(saved.is_trigger_done(trigger));
    }
}

#[tokio::test]
async fn t60_and_t5_due_together_write_once() {
    let mut state = state_with_match(Mode::Write, kickoff());
    state
        .matches
        .get_mut("match-1")
        .expect("match state")
        .record_trigger(Trigger::Publish, now());

    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path);

    let report = runner
        .execute_once(kickoff() - Duration::minutes(4))
        .await
        .expect("execute once");

    assert_eq!(report.triggers, 2);
    assert_eq!(report.writes, 1);
    assert_eq!(report.unchanged, 1);
    assert_eq!(mpp.writes().len(), 1);

    let saved = &runner.state().matches["match-1"];
    assert!(saved.is_trigger_done(Trigger::T60));
    assert!(saved.is_trigger_done(Trigger::T5));
    assert_eq!(saved.written_score(), Some("1-1"));
}

#[tokio::test]
async fn trigger_pass_requires_a_daily_snapshot() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path,
    );

    let error = runner
        .trigger_pass(now())
        .await
        .expect_err("missing snapshot");

    assert!(matches!(error, RunError::MissingSnapshot));
    assert!(!runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
    assert!(mpp.writes().is_empty());
}

#[tokio::test]
async fn permanent_write_failure_marks_the_trigger() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    mpp.fail_writes(StatusCode::BAD_REQUEST);
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path,
    );

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.triggers, 1);
    assert_eq!(report.writes, 0);
    assert_eq!(report.skipped, 1);
    assert!(mpp.writes().is_empty());
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
}

#[tokio::test]
async fn missing_fixture_leaves_the_trigger_pending() {
    let mpp = FakeMpp::new(Snapshot {
        clubs: HashMap::new(),
        matches: Vec::new(),
    });
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path,
    );

    let error = runner
        .execute_once(now())
        .await
        .expect_err("missing fixture");

    assert!(matches!(error, RunError::MissingFixture(_)));
    assert!(error.is_transient());
    assert_eq!(elo.calls(), 0);
    assert!(!runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
    assert!(mpp.writes().is_empty());
}

#[tokio::test]
async fn kickoff_change_resets_pre_match_triggers() {
    let old_kickoff = kickoff();
    let new_kickoff = old_kickoff + Duration::days(1);
    let mut state = state_with_match(Mode::Write, old_kickoff);
    {
        let saved = state.matches.get_mut("match-1").expect("match state");
        saved.record_trigger(Trigger::Publish, now());
        saved.record_trigger(Trigger::T60, now());
    }

    let mpp = FakeMpp::new(snapshot(new_kickoff));
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path);

    let report = runner.execute_once(now()).await.expect("execute once");
    let saved = &runner.state().matches["match-1"];

    assert_eq!(report.triggers, 0);
    assert_eq!(saved.kickoff, new_kickoff);
    assert!(saved.is_trigger_done(Trigger::Publish));
    assert!(!saved.is_trigger_done(Trigger::T60));
}

#[tokio::test]
async fn two_runs_do_not_rewrite_the_same_score() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path,
    );

    runner.execute_once(now()).await.expect("first run");
    let report = runner.execute_once(now()).await.expect("second run");

    assert_eq!(mpp.writes().len(), 1);
    assert_eq!(report.writes, 0);
    assert_eq!(report.triggers, 0);
}

#[tokio::test]
async fn permanent_write_failure_continues_with_other_matches() {
    let mut snapshot = snapshot(kickoff());
    let mut second = fixture(kickoff());
    second.id = "match-2".to_owned();
    snapshot.matches.push(second);

    let mpp = FakeMpp::new(snapshot);
    mpp.fail_writes(StatusCode::BAD_REQUEST);
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");

    let mut state = state_with_match(Mode::Write, kickoff());
    state
        .matches
        .insert("match-2".to_owned(), MatchState::new(kickoff()));
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path);

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.triggers, 2);
    assert_eq!(report.writes, 0);
    assert_eq!(report.skipped, 2);
    assert!(mpp.writes().is_empty());
    for match_id in ["match-1", "match-2"] {
        assert!(runner.state().matches[match_id].is_trigger_done(Trigger::Publish));
    }
}
