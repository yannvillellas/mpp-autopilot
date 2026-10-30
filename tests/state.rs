//! Persisted-state integration tests.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use mpp_autopilot::elo::{EloRatings, MatchResult};
use mpp_autopilot::model::Score;
use mpp_autopilot::state::{MatchState, Mode, State, StateError, Trigger};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mpp-autopilot-state-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn state_path(&self) -> PathBuf {
        self.path.join("state.json")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn at(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, day, hour, 0, 0)
        .single()
        .expect("valid timestamp")
}

fn match_state(kickoff: DateTime<Utc>) -> MatchState {
    let mut state = MatchState::new(kickoff);
    state.record_trigger(Trigger::Publish, at(19, 7));
    state.record_write(Score { home: 1, away: 1 }, at(19, 7));
    state
}

#[test]
fn write_flag_maps_to_mode() {
    assert_eq!(Mode::from_write(true), Mode::Write);
    assert_eq!(Mode::from_write(false), Mode::Dry);
}

#[test]
fn new_state_is_empty_with_mode() {
    let state = State::new(Mode::Dry);

    assert_eq!(state.mode, Mode::Dry);
    assert!(state.matches.is_empty());
}

#[test]
fn missing_file_starts_fresh_in_current_mode() {
    let dir = TestDir::new();
    let state = State::load(&dir.state_path(), Mode::Write).expect("fresh state");

    assert_eq!(state.mode, Mode::Write);
    assert!(state.matches.is_empty());
}

#[test]
fn state_roundtrips_through_atomic_file() {
    let dir = TestDir::new();
    let path = dir.state_path();
    let mut state = State::new(Mode::Write);
    state
        .matches
        .insert("match-1".to_owned(), match_state(at(23, 18)));

    state.save(&path).expect("save state");

    assert!(path.exists());
    assert!(!dir.path().join("state.json.tmp").exists());
    let loaded = State::load(&path, Mode::Write).expect("load state");
    assert_eq!(loaded, state);
}

#[test]
fn elo_cache_roundtrips_through_the_state_file() {
    let dir = TestDir::new();
    let path = dir.state_path();
    let mut state = State::new(Mode::Write);
    state.record_elo(
        EloRatings::from_pairs([("Paris SG", 1900.0), ("Marseille", 1700.0)]),
        at(19, 5),
    );

    state.save(&path).expect("save state");

    let loaded = State::load(&path, Mode::Write).expect("load state");
    let cached = loaded.elo.as_ref().expect("cached ratings");
    assert_eq!(cached.fetched_at, at(19, 5));
    assert_eq!(cached.ratings.rating("PSG"), Some(1900.0));
    assert_eq!(cached.ratings.rating("Marseille"), Some(1700.0));
}

#[test]
fn elo_cache_uses_a_readable_state_schema() {
    let mut state = State::new(Mode::Dry);
    state.record_elo(EloRatings::from_pairs([("Paris SG", 1900.0)]), at(19, 5));

    let json = serde_json::to_value(&state).expect("serialize state");

    assert_eq!(json["elo"]["fetched_at"], "2026-10-19T05:00:00Z");
    assert_eq!(json["elo"]["ratings"]["psg"], 1900.0);
    assert!(json["elo"].get("applied").is_none());
    assert!(json["elo"].get("backfilled_through").is_none());
}

#[test]
fn elo_cache_keeps_applied_matches_across_load() {
    let dir = TestDir::new();
    let path = dir.state_path();
    let outcome = MatchResult {
        season: 2026,
        date: NaiveDate::from_ymd_opt(2026, 10, 17).expect("valid date"),
        home: "Paris SG".to_owned(),
        away: "Marseille".to_owned(),
        home_goals: 1,
        away_goals: 1,
    };
    let mut ratings = EloRatings::from_pairs([("Paris SG", 1800.0), ("Marseille", 1700.0)]);
    assert_eq!(ratings.apply_results([outcome.clone()]), 1);
    let expected = ratings.clone();

    let mut state = State::new(Mode::Write);
    state.record_elo(ratings, at(19, 5));
    state.save(&path).expect("save state");

    let loaded = State::load(&path, Mode::Write).expect("load state");
    let mut restored = loaded.cached_elo();

    assert_eq!(restored, expected);
    assert_eq!(restored.apply_results([outcome]), 0);
    assert_eq!(restored, expected);
}

#[test]
fn legacy_clubelo_rating_keys_still_answer_canonical_lookups() {
    let json = r#"{
      "mode": "dry",
      "matches": {},
      "elo": {
        "fetched_at": "2026-10-19T05:00:00Z",
        "ratings": {
          "paris sg": 1900.0,
          "marseille": 1700.0
        }
      }
    }"#;

    let state: State = serde_json::from_str(json).expect("parse legacy Elo state");
    let cached = state.elo.as_ref().expect("cached ratings");

    assert_eq!(cached.ratings.rating("PSG"), Some(1900.0));
    assert_eq!(state.cached_elo().rating("PSG"), Some(1900.0));
    assert_eq!(state.cached_elo().rating("Marseille"), Some(1700.0));
}

#[test]
fn absent_elo_cache_is_omitted_from_the_state_file() {
    let json = serde_json::to_value(State::new(Mode::Dry)).expect("serialize state");

    assert!(json.get("elo").is_none());
}

#[test]
fn save_creates_missing_parent_directory() {
    let dir = TestDir::new();
    let path = dir.path().join("nested").join("state.json");

    State::new(Mode::Dry).save(&path).expect("save state");

    assert!(path.exists());
}

#[test]
fn invalid_path_has_no_side_effect() {
    let dir = TestDir::new();
    let path = dir.path().join("missing").join("..");

    assert!(matches!(
        State::new(Mode::Dry).save(&path),
        Err(StateError::InvalidPath)
    ));
    assert!(!dir.path().join("missing").exists());
}

#[test]
fn malformed_state_is_rejected() {
    let dir = TestDir::new();
    let path = dir.state_path();
    fs::write(&path, "{ not json").expect("write malformed state");

    assert!(matches!(
        State::load(&path, Mode::Dry),
        Err(StateError::Json(_))
    ));
}

#[test]
fn mode_change_resets_triggers_and_keeps_written_score() {
    let mut state = State::new(Mode::Dry);
    state
        .matches
        .insert("match-1".to_owned(), match_state(at(23, 18)));

    assert!(state.apply_mode(Mode::Write));

    let saved = &state.matches["match-1"];
    assert_eq!(state.mode, Mode::Write);
    assert!(saved.triggers.is_empty());
    assert_eq!(saved.written_score(), Some("1-1"));
    assert_eq!(saved.last_written_at, Some(at(19, 7)));
    assert!(!state.apply_mode(Mode::Write));
}

#[test]
fn loading_with_a_different_mode_resets_triggers() {
    let dir = TestDir::new();
    let path = dir.state_path();
    let mut state = State::new(Mode::Dry);
    state
        .matches
        .insert("match-1".to_owned(), match_state(at(23, 18)));
    state.save(&path).expect("save state");

    let loaded = State::load(&path, Mode::Write).expect("load state");

    assert_eq!(loaded.mode, Mode::Write);
    assert!(loaded.matches["match-1"].triggers.is_empty());
    assert_eq!(loaded.matches["match-1"].written_score(), Some("1-1"));
}

#[test]
fn loading_with_the_same_mode_keeps_triggers() {
    let dir = TestDir::new();
    let path = dir.state_path();
    let mut state = State::new(Mode::Write);
    state
        .matches
        .insert("match-1".to_owned(), match_state(at(23, 18)));
    state.save(&path).expect("save state");

    let loaded = State::load(&path, Mode::Write).expect("load state");

    assert_eq!(
        loaded.matches["match-1"].triggers.get(&Trigger::Publish),
        Some(&at(19, 7))
    );
}

#[test]
fn purge_missing_removes_only_matches_absent_from_the_snapshot() {
    let mut state = State::new(Mode::Dry);
    for id in ["listed", "absent"] {
        state
            .matches
            .insert(id.to_owned(), MatchState::new(at(19, 12)));
    }

    let live = BTreeSet::from(["listed".to_owned()]);
    state.purge_missing(&live);

    assert!(state.matches.contains_key("listed"));
    assert!(!state.matches.contains_key("absent"));
}

#[test]
fn purge_missing_keeps_a_finished_match_and_its_write_record() {
    let mut state = State::new(Mode::Write);
    state
        .matches
        .insert("finished".to_owned(), match_state(at(19, 18)));

    state.purge_missing(&BTreeSet::from(["finished".to_owned()]));

    let saved = &state.matches["finished"];
    assert_eq!(saved.written_score(), Some("1-1"));
    assert_eq!(saved.last_written_at, Some(at(19, 7)));
}

#[test]
fn triggers_are_recorded_once() {
    let mut state = MatchState::new(at(23, 18));

    assert!(state.record_trigger(Trigger::Publish, at(19, 7)));
    assert!(!state.record_trigger(Trigger::Publish, at(19, 8)));
    assert_eq!(state.triggers.get(&Trigger::Publish), Some(&at(19, 7)));
}

#[test]
fn trigger_names_and_score_match_the_state_schema() {
    let mut state = State::new(Mode::Write);
    let mut match_state = match_state(at(23, 18));
    match_state.record_trigger(Trigger::T60, at(23, 17));
    match_state.record_trigger(Trigger::T5, at(23, 18));
    match_state.record_trigger(Trigger::T1, at(23, 18));
    state.matches.insert("match-1".to_owned(), match_state);

    let json = serde_json::to_value(&state).expect("serialize state");
    let match_json = &json["matches"]["match-1"];
    let triggers = match_json["triggers"].as_object().expect("triggers object");

    assert_eq!(json["mode"], "write");
    assert_eq!(match_json["last_written_score"], "1-1");
    assert!(triggers.contains_key("publish"));
    assert!(triggers.contains_key("t60"));
    assert!(triggers.contains_key("t5"));
    assert!(triggers.contains_key("t1"));
}

#[test]
fn state_written_before_t1_still_loads_and_keeps_its_trigger_keys() {
    let json = r#"{
      "mode": "write",
      "matches": {
        "match-1": {
          "kickoff": "2026-10-23T18:45:00Z",
          "triggers": {
            "publish": "2026-10-19T05:00:00Z",
            "t60": "2026-10-23T17:45:00Z",
            "t5": "2026-10-23T18:40:00Z"
          }
        }
      }
    }"#;

    let state: State = serde_json::from_str(json).expect("parse state written before T-1");
    let saved = &state.matches["match-1"];

    assert_eq!(saved.triggers.get(&Trigger::Publish), Some(&at(19, 5)));
    assert_eq!(
        saved.triggers.get(&Trigger::T60),
        Some(&(at(23, 17) + Duration::minutes(45)))
    );
    assert_eq!(
        saved.triggers.get(&Trigger::T5),
        Some(&(at(23, 18) + Duration::minutes(40)))
    );
    assert!(!saved.is_trigger_done(Trigger::T1));

    let json = serde_json::to_value(&state).expect("serialize state");
    let triggers = json["matches"]["match-1"]["triggers"]
        .as_object()
        .expect("triggers object");

    assert_eq!(triggers.len(), 3);
    assert!(triggers.contains_key("publish"));
    assert!(triggers.contains_key("t60"));
    assert!(triggers.contains_key("t5"));
    assert!(!triggers.contains_key("t1"));
}

#[test]
fn legacy_state_keys_are_still_accepted() {
    let json = r#"
    {
      "mode": "write",
      "matches": {
        "mpp_championship_match_2640624": {
          "kickoff": "2026-10-23T18:45:00+00:00",
          "triggers": {
            "publish": "2026-10-19T05:00:00+00:00",
            "t60": "2026-10-23T17:45:00+00:00",
            "t5": "2026-10-23T18:40:00+00:00"
          },
          "score": "1-1",
          "written_at": "2026-10-19T05:00:01+00:00"
        }
      }
    }"#;

    let state: State = serde_json::from_str(json).expect("parse legacy state");
    let saved = &state.matches["mpp_championship_match_2640624"];

    assert_eq!(state.mode, Mode::Write);
    assert_eq!(
        saved.kickoff,
        Utc.with_ymd_and_hms(2026, 10, 23, 18, 45, 0)
            .single()
            .expect("valid kickoff")
    );
    assert_eq!(saved.written_score(), Some("1-1"));
    assert_eq!(
        saved.last_written_at,
        Some(at(19, 5) + Duration::seconds(1))
    );
    assert_eq!(saved.triggers.len(), 3);
    assert_eq!(state.elo, None);
}

#[test]
fn minimal_match_state_defaults_optional_fields() {
    let json = r#"{
      "mode": "dry",
      "matches": {
        "match-1": { "kickoff": "2026-10-23T18:45:00Z" }
      }
    }"#;

    let state: State = serde_json::from_str(json).expect("parse minimal state");
    let saved = &state.matches["match-1"];

    assert!(saved.triggers.is_empty());
    assert_eq!(saved.last_written_score, None);
    assert_eq!(saved.last_written_at, None);
}
