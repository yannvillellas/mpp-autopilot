//! Orchestrator behavior with fake MPP and Elo clients.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use chrono::{DateTime, Duration, TimeZone, Utc};
use mpp_autopilot::elo::{EloError, EloRatings, EloSite};
use mpp_autopilot::model::{self, Lambdas, Odds, Score};
use mpp_autopilot::mpp::{
    Club, Fixture, ForecastHistory, HistoricalForecast, HistoricalMatch, HistoryPoints, MppApi,
    MppError, Snapshot,
};
use mpp_autopilot::odds::{MarketForecast, OddsError, OddsSite};
use mpp_autopilot::run::{Orchestrator, RunError};
use mpp_autopilot::state::{
    ForecastOrigin, ForecastRecord, MatchState, Mode, SeasonMatch, State, Trigger,
};
use reqwest::StatusCode;

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mpp-autopilot-run-{}-{}",
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
    forecasts: Mutex<HashMap<String, Score>>,
    fail_status: Mutex<Option<FakeWriteFailure>>,
    fail_reads: Mutex<Option<StatusCode>>,
    lose_write_response: Mutex<bool>,
    reads: AtomicUsize,
    history: Mutex<ForecastHistory>,
    fail_history: AtomicBool,
}

struct FakeWriteFailure {
    match_id: Option<String>,
    status: StatusCode,
    body: String,
}

impl FakeMpp {
    fn new(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            writes: Mutex::new(Vec::new()),
            forecasts: Mutex::new(HashMap::new()),
            fail_status: Mutex::new(None),
            fail_reads: Mutex::new(None),
            lose_write_response: Mutex::new(false),
            reads: AtomicUsize::new(0),
            history: Mutex::new(ForecastHistory::default()),
            fail_history: AtomicBool::new(false),
        }
    }

    fn fail_writes(&self, status: StatusCode) {
        self.fail_writes_with_body(status, "forced write failure");
    }

    fn fail_writes_for(&self, match_id: &str, status: StatusCode) {
        self.set_write_failure(
            Some(match_id.to_owned()),
            status,
            "forced write failure".to_owned(),
        );
    }

    fn fail_writes_with_body(&self, status: StatusCode, body: impl Into<String>) {
        self.set_write_failure(None, status, body.into());
    }

    fn set_write_failure(&self, match_id: Option<String>, status: StatusCode, body: String) {
        *self.fail_status.lock().expect("fail status lock") = Some(FakeWriteFailure {
            match_id,
            status,
            body,
        });
    }

    fn fail_reads(&self, status: StatusCode) {
        *self.fail_reads.lock().expect("fail reads lock") = Some(status);
    }

    /// Records the next write but returns a transient error, simulating a
    /// successful PATCH whose response or state save was lost.
    fn lose_next_write_response(&self) {
        *self
            .lose_write_response
            .lock()
            .expect("lose write response lock") = true;
    }

    /// Simulates a forecast already registered in MPP, for example one the
    /// user edited by hand in the app.
    fn preset_forecast(&self, match_id: &str, score: Score) {
        self.forecasts
            .lock()
            .expect("forecasts lock")
            .insert(match_id.to_owned(), score);
    }

    fn writes(&self) -> Vec<(String, Score)> {
        self.writes.lock().expect("writes lock").clone()
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    fn set_history(&self, history: ForecastHistory) {
        *self.history.lock().expect("history lock") = history;
    }

    fn fail_history(&self) {
        self.fail_history.store(true, Ordering::Relaxed);
    }
}

impl MppApi for FakeMpp {
    async fn fetch_snapshot(&self) -> Result<Snapshot, MppError> {
        Ok(self.snapshot.clone())
    }

    async fn fetch_forecast(&self, match_id: &str) -> Result<Option<Score>, MppError> {
        self.reads.fetch_add(1, Ordering::Relaxed);

        if let Some(status) = *self.fail_reads.lock().expect("fail reads lock") {
            return Err(MppError::Status {
                status,
                body: "forced read failure".to_owned(),
            });
        }

        Ok(self
            .forecasts
            .lock()
            .expect("forecasts lock")
            .get(match_id)
            .copied())
    }

    async fn fetch_user_id(&self) -> Result<String, MppError> {
        Ok("user-1".to_owned())
    }

    async fn fetch_forecast_history(
        &self,
        _user_id: &str,
        _before_date: Option<&str>,
    ) -> Result<ForecastHistory, MppError> {
        if self.fail_history.load(Ordering::Relaxed) {
            return Err(MppError::Status {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: "forced history failure".to_owned(),
            });
        }
        Ok(self.history.lock().expect("history lock").clone())
    }

    async fn submit_forecast(&self, match_id: &str, score: Score) -> Result<(), MppError> {
        let fail = self.fail_status.lock().expect("fail status lock");
        if let Some(failure) = fail.as_ref() {
            if failure
                .match_id
                .as_deref()
                .is_none_or(|target| target == match_id)
            {
                return Err(MppError::Status {
                    status: failure.status,
                    body: failure.body.clone(),
                });
            }
        }
        drop(fail);

        self.writes
            .lock()
            .expect("writes lock")
            .push((match_id.to_owned(), score));
        self.forecasts
            .lock()
            .expect("forecasts lock")
            .insert(match_id.to_owned(), score);

        let lose_response = std::mem::take(
            &mut *self
                .lose_write_response
                .lock()
                .expect("lose write response lock"),
        );
        if lose_response {
            return Err(MppError::Status {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: "forced lost response".to_owned(),
            });
        }

        Ok(())
    }
}

struct FakeElo {
    ratings: EloRatings,
    calls: AtomicUsize,
    fail: AtomicBool,
}

impl FakeElo {
    fn new(ratings: EloRatings) -> Self {
        Self {
            ratings,
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        }
    }

    fn fail(&self) {
        self.fail.store(true, Ordering::Relaxed);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl EloSite for FakeElo {
    async fn fetch_ratings(&self) -> Result<EloRatings, EloError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fail.load(Ordering::Relaxed) {
            return Err(EloError::Status {
                status: StatusCode::FORBIDDEN,
                body: "forced ratings failure".to_owned(),
            });
        }
        Ok(self.ratings.clone())
    }
}

struct FakeOdds {
    forecasts: Mutex<Vec<MarketForecast>>,
    calls: AtomicUsize,
    fail: AtomicBool,
}

impl FakeOdds {
    fn new(forecasts: Vec<MarketForecast>) -> Self {
        Self {
            forecasts: Mutex::new(forecasts),
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        }
    }

    fn empty() -> Self {
        Self::new(Vec::new())
    }

    fn fail(&self) {
        self.fail.store(true, Ordering::Relaxed);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl OddsSite for FakeOdds {
    async fn fetch_forecasts(&self) -> Result<Vec<MarketForecast>, OddsError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fail.load(Ordering::Relaxed) {
            return Err(OddsError::Status {
                status: StatusCode::BAD_REQUEST,
                body: "forced odds failure".to_owned(),
            });
        }
        Ok(self.forecasts.lock().expect("forecasts lock").clone())
    }
}

fn market_forecast(home: &str, away: &str, lambdas: Lambdas) -> MarketForecast {
    MarketForecast {
        commence_time: kickoff(),
        home_team: home.to_owned(),
        away_team: away.to_owned(),
        lambdas,
        observed_at: now(),
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
        vec![("match-1".to_owned(), Score { home: 1, away: 2 })]
    );

    let saved = &runner.state().matches["match-1"];
    assert!(saved.is_trigger_done(Trigger::Publish));
    assert_eq!(saved.written_score(), Some("1-2"));
    assert_eq!(saved.last_written_at, Some(now()));
    assert_eq!(elo.calls(), 1);
    assert_eq!(mpp.reads(), 1);

    let loaded = State::load(&path, Mode::Write).expect("load state");
    assert_eq!(loaded.matches["match-1"].written_score(), Some("1-2"));
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
    assert_eq!(mpp.reads(), 0);
    assert!(mpp.writes().is_empty());

    let saved = &runner.state().matches["match-1"];
    assert!(saved.is_trigger_done(Trigger::Publish));
    assert_eq!(saved.written_score(), None);
    assert_eq!(saved.last_written_at, None);
}

#[tokio::test]
async fn matching_remote_forecast_is_not_rewritten() {
    // The state records no write at all: only the forecast read back from MPP
    // can tell that the pick is already in place.
    let mut state = state_with_match(Mode::Write, kickoff());
    state
        .matches
        .get_mut("match-1")
        .expect("match state")
        .record_trigger(Trigger::Publish, now());

    let mpp = FakeMpp::new(snapshot(kickoff()));
    mpp.preset_forecast("match-1", Score { home: 1, away: 2 });
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
    assert_eq!(mpp.reads(), 1);
    assert!(mpp.writes().is_empty());
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::T60));
}

#[tokio::test]
async fn manual_forecast_edit_is_overwritten() {
    // The bot wrote 1-2 earlier, the user then edited the forecast to 0-1 in
    // the app: the remote score wins over the local audit trail.
    let mut state = state_with_match(Mode::Write, kickoff());
    {
        let saved = state.matches.get_mut("match-1").expect("match state");
        saved.record_trigger(Trigger::Publish, now());
        saved.record_write(Score { home: 1, away: 2 }, now());
    }

    let mpp = FakeMpp::new(snapshot(kickoff()));
    mpp.preset_forecast("match-1", Score { home: 0, away: 1 });
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path);

    let report = runner
        .execute_once(kickoff() - Duration::minutes(60))
        .await
        .expect("execute once");

    assert_eq!(report.triggers, 1);
    assert_eq!(report.writes, 1);
    assert_eq!(report.unchanged, 0);
    assert_eq!(mpp.reads(), 1);
    assert_eq!(
        mpp.writes(),
        vec![("match-1".to_owned(), Score { home: 1, away: 2 })]
    );
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::T60));
}

#[tokio::test]
async fn read_failures_still_write() {
    for status in [StatusCode::INTERNAL_SERVER_ERROR, StatusCode::BAD_REQUEST] {
        let mpp = FakeMpp::new(snapshot(kickoff()));
        mpp.fail_reads(status);
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

        assert_eq!(report.triggers, 1, "status {status}");
        assert_eq!(report.writes, 1, "status {status}");
        assert_eq!(report.skipped, 0, "status {status}");
        assert_eq!(mpp.reads(), 1, "status {status}");
        assert_eq!(
            mpp.writes(),
            vec![("match-1".to_owned(), Score { home: 1, away: 2 })],
            "status {status}"
        );
        assert!(
            runner.state().matches["match-1"].is_trigger_done(Trigger::Publish),
            "status {status}"
        );
    }
}

#[tokio::test]
async fn lost_write_response_is_recovered_without_a_duplicate() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    mpp.lose_next_write_response();
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

    runner
        .execute_once(now())
        .await
        .expect_err("lost write response");

    assert_eq!(mpp.writes().len(), 1);
    assert_eq!(mpp.reads(), 1);
    assert!(!runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));

    let report = runner.execute_once(now()).await.expect("retry");

    assert_eq!(report.triggers, 1);
    assert_eq!(report.writes, 0);
    assert_eq!(report.unchanged, 1);
    assert_eq!(mpp.writes().len(), 1);
    assert_eq!(mpp.reads(), 2);
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
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
    assert_eq!(mpp.reads(), 1);
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

    assert_eq!(report.triggers, 0);
    assert_eq!(report.finished, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.writes, 0);
    assert_eq!(elo.calls(), 0);
    assert!(mpp.writes().is_empty());

    let saved = &runner.state().matches["match-1"];
    for trigger in Trigger::ALL {
        assert!(saved.is_trigger_done(trigger));
    }
}

#[tokio::test]
async fn t1_after_kickoff_is_marked_without_fetching() {
    let past_kickoff = now() - Duration::minutes(1);
    let mut state = state_with_match(Mode::Write, past_kickoff);
    {
        let saved = state.matches.get_mut("match-1").expect("match state");
        saved.record_trigger(Trigger::Publish, now() - Duration::days(1));
        saved.record_trigger(Trigger::T60, now() - Duration::hours(1));
        saved.record_trigger(Trigger::T5, now() - Duration::minutes(5));
    }

    let mpp = FakeMpp::new(snapshot(past_kickoff));
    let elo = FakeElo::new(ratings());
    let market = FakeOdds::empty();
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path).with_market(&market);

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.triggers, 0);
    assert_eq!(report.finished, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(market.calls(), 0);
    assert_eq!(elo.calls(), 0);
    assert!(mpp.writes().is_empty());
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::T1));
}

#[tokio::test]
async fn t1_due_one_minute_before_kickoff_is_marked() {
    let kickoff = now() + Duration::minutes(1);
    let mut state = state_with_match(Mode::Write, kickoff);
    {
        let saved = state.matches.get_mut("match-1").expect("match state");
        saved.record_trigger(Trigger::Publish, now() - Duration::days(1));
        saved.record_trigger(Trigger::T60, now() - Duration::hours(1));
        saved.record_trigger(Trigger::T5, now() - Duration::minutes(5));
    }

    let mpp = FakeMpp::new(snapshot(kickoff));
    let elo = FakeElo::new(ratings());
    let market = FakeOdds::empty();
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path).with_market(&market);

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.triggers, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(market.calls(), 1);
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::T1));
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
    assert_eq!(mpp.reads(), 2);
    assert_eq!(mpp.writes().len(), 1);

    let saved = &runner.state().matches["match-1"];
    assert!(saved.is_trigger_done(Trigger::T60));
    assert!(saved.is_trigger_done(Trigger::T5));
    assert_eq!(saved.written_score(), Some("1-2"));
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
    assert_eq!(mpp.reads(), 1);
    assert!(mpp.writes().is_empty());
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
}

#[tokio::test]
async fn kickoff_lock_marks_the_trigger_without_a_pass_error() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    mpp.fail_writes_with_body(
        StatusCode::FORBIDDEN,
        r#"{"error":"matchForecastsNotAllowed"}"#,
    );
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

    let report = runner
        .execute_once(now())
        .await
        .expect("kickoff lock is expected");

    assert_eq!(report.triggers, 1);
    assert_eq!(report.writes, 0);
    assert_eq!(report.skipped, 1);
    assert_eq!(mpp.reads(), 1);
    assert!(mpp.writes().is_empty());
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
}

#[tokio::test]
async fn other_forbidden_responses_still_fail_the_pass() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    mpp.fail_writes_with_body(StatusCode::FORBIDDEN, r#"{"error":"token revoked"}"#);
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
        .expect_err("forbidden is not a kickoff lock");

    assert!(matches!(
        error,
        RunError::Mpp(MppError::Status { status, .. }) if status == StatusCode::FORBIDDEN
    ));
    assert!(!runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
    assert!(mpp.writes().is_empty());
}

#[tokio::test]
async fn missing_fixture_is_purged_without_blocking() {
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

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.matches, 0);
    assert_eq!(report.triggers, 0);
    assert_eq!(report.finished, 0);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.writes, 0);
    assert_eq!(elo.calls(), 0);
    assert!(!runner.state().matches.contains_key("match-1"));
    assert!(mpp.writes().is_empty());
}

#[tokio::test]
async fn missing_fixture_does_not_block_other_matches() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");

    let mut state = state_with_match(Mode::Write, kickoff());
    state
        .matches
        .insert("stale-match".to_owned(), MatchState::new(kickoff()));
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path);

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.matches, 1);
    assert_eq!(report.triggers, 1);
    assert_eq!(report.writes, 1);
    assert_eq!(report.finished, 0);
    assert_eq!(report.skipped, 0);
    assert_eq!(elo.calls(), 1);
    assert_eq!(
        mpp.writes(),
        vec![("match-1".to_owned(), Score { home: 1, away: 2 })]
    );
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
    assert!(!runner.state().matches.contains_key("stale-match"));
}

#[tokio::test]
async fn transient_write_failure_does_not_block_other_matches() {
    let mut snapshot = snapshot(kickoff());
    let mut second = fixture(kickoff());
    second.id = "match-2".to_owned();
    snapshot.matches.push(second);

    let mpp = FakeMpp::new(snapshot);
    mpp.fail_writes_for("match-1", StatusCode::INTERNAL_SERVER_ERROR);
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");

    let mut state = state_with_match(Mode::Write, kickoff());
    state
        .matches
        .insert("match-2".to_owned(), MatchState::new(kickoff()));
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path);

    let error = runner
        .execute_once(now())
        .await
        .expect_err("transient failure");

    assert!(matches!(error, RunError::Mpp(_)));
    assert_eq!(
        mpp.writes(),
        vec![("match-2".to_owned(), Score { home: 1, away: 2 })]
    );
    assert!(!runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
    assert!(runner.state().matches["match-2"].is_trigger_done(Trigger::Publish));
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
        saved.record_trigger(Trigger::T1, now());
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
    assert!(!saved.is_trigger_done(Trigger::T1));
}

#[tokio::test]
async fn completed_triggers_are_not_replayed() {
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
async fn finished_match_is_not_recreated_blank_every_morning() {
    let past = now() - Duration::days(4);
    let mut state = state_with_match(Mode::Write, past);
    state
        .matches
        .get_mut("match-1")
        .expect("match state")
        .record_write(Score { home: 2, away: 0 }, now() - Duration::days(4));

    let mpp = FakeMpp::new(snapshot(past));
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path);

    let first = runner.execute_once(now()).await.expect("first pass");
    assert_eq!(first.finished, 1);
    assert_eq!(first.triggers, 0);
    assert_eq!(first.skipped, 0);
    assert_eq!(
        runner.state().matches["match-1"].written_score(),
        Some("2-0")
    );

    let second = runner
        .execute_once(now() + Duration::days(1))
        .await
        .expect("second pass");
    assert_eq!(second.finished, 0);
    assert_eq!(second.triggers, 0);
    assert_eq!(second.skipped, 0);
    assert_eq!(
        runner.state().matches["match-1"].written_score(),
        Some("2-0")
    );
    for trigger in Trigger::ALL {
        assert!(
            runner.state().matches["match-1"].is_trigger_done(trigger),
            "{trigger:?} should stay closed"
        );
    }
}

#[tokio::test]
async fn history_result_is_merged_into_the_season_ledger() {
    let past = now() - Duration::days(4);
    let scored_odds = Odds {
        home: 48.0,
        draw: 155.0,
        away: 30.0,
    };
    let mut state = state_with_match(Mode::Write, past);
    state
        .matches
        .get_mut("match-1")
        .expect("match state")
        .forecast = Some(ForecastRecord {
        pick: Score { home: 2, away: 0 },
        odds: scored_odds,
        source: ForecastOrigin::Market,
        observed_at: now(),
        lambdas: Lambdas {
            home: 1.5,
            away: 1.2,
        },
        modal: Score { home: 2, away: 1 },
        modal_probability: 0.1,
    });

    let mpp = FakeMpp::new(snapshot(past));
    mpp.set_history(ForecastHistory {
        matches: vec![HistoricalMatch {
            id: "match-1".to_owned(),
            kickoff: past,
            home_club_id: "club_psg".to_owned(),
            away_club_id: "club_om".to_owned(),
            home_score: Some(2),
            away_score: Some(0),
            period: Some("fullTime".to_owned()),
            match_status: Some("decided".to_owned()),
            quotations: Some(scored_odds),
            forecast: Some(HistoricalForecast {
                pick: Score { home: 2, away: 0 },
                points: Some(HistoryPoints {
                    base: Some(48),
                    exact: Some(30),
                    extra: Some(0),
                    bonus: Some(0),
                    total: Some(78),
                    rarity_level: None,
                }),
                double_points: false,
            }),
        }],
        next_before_date: None,
    });
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path);

    let report = runner.daily_pass(now()).await.expect("daily pass");

    assert_eq!(report.results, 1);
    let row = &runner.state().season_matches["match-1"];
    assert_eq!(row.pick, Some(Score { home: 2, away: 0 }));
    assert_eq!(row.result, Some(Score { home: 2, away: 0 }));
    assert_eq!(row.points.as_ref().expect("points").total, Some(78));
    assert_eq!(row.computed_points, Some(78.0));
    assert_eq!(row.modal_points, Some(scored_odds.home));
    assert_eq!(row.source, Some(ForecastOrigin::Market));
    assert_eq!(row.home, "club_psg");
    assert_eq!(row.away, "club_om");
}

#[tokio::test]
async fn history_failure_does_not_fail_the_daily_pass() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    mpp.fail_history();
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

    let report = runner.daily_pass(now()).await.expect("daily pass");

    assert_eq!(report.matches, 1);
    assert_eq!(report.results, 0);
    assert_eq!(runner.state().user_id.as_deref(), Some("user-1"));
}

#[tokio::test]
async fn a_result_closes_pending_triggers_without_a_write() {
    let future = kickoff();
    let mut state = state_with_match(Mode::Write, future);
    state.season_matches.insert(
        "match-1".to_owned(),
        SeasonMatch {
            match_id: "match-1".to_owned(),
            kickoff: future,
            season: SeasonMatch::season_from_kickoff(future),
            home: "Paris SG".to_owned(),
            away: "Marseille".to_owned(),
            pick: Some(Score { home: 2, away: 0 }),
            result: Some(Score { home: 2, away: 0 }),
            points: None,
            computed_points: None,
            modal_points: None,
            source: None,
            odds: None,
            modal: None,
            status: Some("decided".to_owned()),
            observed_at: now(),
        },
    );

    let mpp = FakeMpp::new(snapshot(future));
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path);

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.finished, 1);
    assert_eq!(report.writes, 0);
    assert!(mpp.writes().is_empty());
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

#[tokio::test]
async fn elo_fetch_failure_uses_the_last_successful_ratings() {
    let fetched_at = now() - Duration::days(2);
    let mut state = state_with_match(Mode::Write, kickoff());
    state.record_elo(ratings(), fetched_at);

    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(EloRatings::default());
    elo.fail();
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path.clone());

    let report = runner.execute_once(now()).await.expect("cached ratings");

    assert_eq!(report.triggers, 1);
    assert_eq!(report.writes, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(elo.calls(), 1);
    assert_eq!(
        mpp.writes(),
        vec![("match-1".to_owned(), Score { home: 1, away: 2 })]
    );
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));

    let cached = runner.state().elo.as_ref().expect("cached ratings");
    assert_eq!(cached.fetched_at, fetched_at);
    assert_eq!(cached.ratings, ratings());

    let loaded = State::load(&path, Mode::Write).expect("load state");
    assert_eq!(
        loaded.elo.as_ref().expect("persisted cache").fetched_at,
        fetched_at
    );
}

#[tokio::test]
async fn elo_fetch_failure_without_a_cache_fails_the_pass() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    elo.fail();
    let market = FakeOdds::empty();
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path,
    )
    .with_market(&market);

    let error = runner.execute_once(now()).await.expect_err("no cache");

    assert!(matches!(error, RunError::Elo(_)));
    assert_eq!(elo.calls(), 1);
    assert_eq!(market.calls(), 1);
    assert_eq!(mpp.reads(), 0);
    assert!(mpp.writes().is_empty());
    assert!(!runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
}

#[tokio::test]
async fn empty_cached_ratings_are_treated_as_absent() {
    let mut state = state_with_match(Mode::Write, kickoff());
    state.record_elo(EloRatings::default(), now() - Duration::days(1));

    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    elo.fail();
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path);

    let error = runner.execute_once(now()).await.expect_err("empty cache");

    assert!(matches!(error, RunError::Elo(_)));
    assert!(mpp.writes().is_empty());
    assert!(!runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
}

#[tokio::test]
async fn incomplete_cached_ratings_leave_the_trigger_pending() {
    let mut state = state_with_match(Mode::Write, kickoff());
    state.record_elo(
        EloRatings::from_pairs([("Paris SG", 1684.0)]),
        now() - Duration::days(1),
    );

    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    elo.fail();
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path);

    let error = runner
        .execute_once(now())
        .await
        .expect_err("incomplete cache");

    assert!(matches!(error, RunError::CachedEloIncomplete));
    assert!(mpp.writes().is_empty());
    assert!(!runner.state().matches["match-1"].is_trigger_done(Trigger::Publish));
}

#[tokio::test]
async fn successful_elo_fetch_is_cached_for_later_failures() {
    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut state = state_with_match(Mode::Write, kickoff());
    state.record_elo(
        EloRatings::from_pairs([("Paris SG", 1000.0), ("Marseille", 1000.0)]),
        now() - Duration::days(30),
    );
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path.clone());

    runner.execute_once(now()).await.expect("first run");

    let cached = runner.state().elo.as_ref().expect("cached ratings");
    assert_eq!(cached.fetched_at, now());
    assert_eq!(cached.ratings, ratings());

    let loaded = State::load(&path, Mode::Write).expect("load state");
    elo.fail();
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, loaded, path);

    let report = runner
        .execute_once(kickoff() - Duration::minutes(60))
        .await
        .expect("cached ratings after restart");

    assert_eq!(report.triggers, 1);
    assert_eq!(report.writes, 0);
    assert_eq!(report.unchanged, 1);
    assert!(runner.state().matches["match-1"].is_trigger_done(Trigger::T60));
}

#[tokio::test]
async fn market_odds_are_used_before_elo() {
    let market_lambdas = Lambdas {
        home: 2.4,
        away: 0.6,
    };
    let market_pick =
        model::predict_from_lambdas(market_lambdas, odds()).expect("market prediction");
    let elo_pick =
        model::predict(1684.0983240112262, 1670.9520303480226, odds()).expect("Elo prediction");
    assert_ne!(market_pick.pick, elo_pick.pick);

    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    elo.fail();
    let market = FakeOdds::new(vec![market_forecast(
        "Paris SG",
        "Marseille",
        market_lambdas,
    )]);
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path,
    )
    .with_market(&market);

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.writes, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(mpp.writes(), vec![("match-1".to_owned(), market_pick.pick)]);
    assert_eq!(market.calls(), 1);
    assert_eq!(elo.calls(), 1);
}

#[tokio::test]
async fn fixture_without_market_odds_falls_back_to_elo() {
    let elo_pick =
        model::predict(1684.0983240112262, 1670.9520303480226, odds()).expect("Elo prediction");
    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    let market = FakeOdds::empty();
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path,
    )
    .with_market(&market);

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.writes, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(mpp.writes(), vec![("match-1".to_owned(), elo_pick.pick)]);
    assert_eq!(market.calls(), 1);
    assert_eq!(elo.calls(), 1);
}

#[tokio::test]
async fn market_request_failure_falls_back_to_elo() {
    let elo_pick =
        model::predict(1684.0983240112262, 1670.9520303480226, odds()).expect("Elo prediction");
    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    let market = FakeOdds::empty();
    market.fail();
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path,
    )
    .with_market(&market);

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.writes, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(mpp.writes(), vec![("match-1".to_owned(), elo_pick.pick)]);
    assert_eq!(market.calls(), 1);
    assert_eq!(elo.calls(), 1);
}

#[tokio::test]
async fn missing_market_odds_use_cached_elo() {
    let fetched_at = now() - Duration::days(2);
    let mut state = state_with_match(Mode::Write, kickoff());
    state.record_elo(ratings(), fetched_at);
    let elo_pick =
        model::predict(1684.0983240112262, 1670.9520303480226, odds()).expect("Elo prediction");

    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(EloRatings::default());
    elo.fail();
    let market = FakeOdds::empty();
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path).with_market(&market);

    let report = runner.execute_once(now()).await.expect("cached ratings");

    assert_eq!(report.writes, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(mpp.writes(), vec![("match-1".to_owned(), elo_pick.pick)]);
    assert_eq!(market.calls(), 1);
    assert_eq!(elo.calls(), 1);
}

#[tokio::test]
async fn one_market_request_serves_every_fixture_in_the_pass() {
    let mut snapshot = snapshot(kickoff());
    let mut second = fixture(kickoff());
    second.id = "match-2".to_owned();
    snapshot.matches.push(second);

    let market_lambdas = Lambdas {
        home: 2.4,
        away: 0.6,
    };
    let market_pick =
        model::predict_from_lambdas(market_lambdas, odds()).expect("market prediction");
    let mpp = FakeMpp::new(snapshot);
    let elo = FakeElo::new(ratings());
    let market = FakeOdds::new(vec![market_forecast(
        "Paris SG",
        "Marseille",
        market_lambdas,
    )]);
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut state = state_with_match(Mode::Write, kickoff());
    state
        .matches
        .insert("match-2".to_owned(), MatchState::new(kickoff()));
    let mut runner = Orchestrator::new(&mpp, &elo, Mode::Write, state, path).with_market(&market);

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.writes, 2);
    assert_eq!(market.calls(), 1);
    assert_eq!(
        mpp.writes(),
        vec![
            ("match-1".to_owned(), market_pick.pick),
            ("match-2".to_owned(), market_pick.pick),
        ]
    );
}

#[tokio::test]
async fn a_market_event_five_hours_away_is_matched() {
    let market_lambdas = Lambdas {
        home: 2.4,
        away: 0.6,
    };
    let market_pick =
        model::predict_from_lambdas(market_lambdas, odds()).expect("market prediction");
    let mut event = market_forecast("Paris SG", "Marseille", market_lambdas);
    event.commence_time = kickoff() + Duration::hours(5);

    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    let market = FakeOdds::new(vec![event]);
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path,
    )
    .with_market(&market);

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.writes, 1);
    assert_eq!(mpp.writes(), vec![("match-1".to_owned(), market_pick.pick)]);
}

#[tokio::test]
async fn a_market_event_seven_hours_away_is_not_matched() {
    let elo_pick =
        model::predict(1684.0983240112262, 1670.9520303480226, odds()).expect("Elo prediction");
    let mut event = market_forecast(
        "Paris SG",
        "Marseille",
        Lambdas {
            home: 2.4,
            away: 0.6,
        },
    );
    event.commence_time = kickoff() + Duration::hours(7);

    let mpp = FakeMpp::new(snapshot(kickoff()));
    let elo = FakeElo::new(ratings());
    let market = FakeOdds::new(vec![event]);
    let dir = TestDir::new();
    let path = dir.path().join("state.json");
    let mut runner = Orchestrator::new(
        &mpp,
        &elo,
        Mode::Write,
        state_with_match(Mode::Write, kickoff()),
        path,
    )
    .with_market(&market);

    let report = runner.execute_once(now()).await.expect("execute once");

    assert_eq!(report.writes, 1);
    assert_eq!(mpp.writes(), vec![("match-1".to_owned(), elo_pick.pick)]);
}
