//! Persistent state for match triggers and submitted scores.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::Score;

/// Matches whose kickoff is older than this many days are purged.
pub const PURGE_AFTER_DAYS: i64 = 3;

/// Execution mode persisted with the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Compute and log without writing to MPP.
    Dry,
    /// Write predictions to MPP.
    Write,
}

impl Mode {
    /// Maps the `WRITE` configuration flag to a persisted mode.
    pub fn from_write(write: bool) -> Self {
        if write { Self::Write } else { Self::Dry }
    }
}

/// Per-match trigger that may run once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Trigger {
    /// First daily pass where the match is seen.
    #[serde(rename = "publish")]
    Publish,
    /// Sixty minutes before kickoff.
    #[serde(rename = "t60")]
    T60,
    /// Five minutes before kickoff.
    #[serde(rename = "t5")]
    T5,
}

impl Trigger {
    /// All triggers, in execution order.
    pub const ALL: [Self; 3] = [Self::Publish, Self::T60, Self::T5];
}

/// Persisted state shared by all matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Last execution mode.
    pub mode: Mode,
    /// Match state, keyed by MPP match id.
    pub matches: BTreeMap<String, MatchState>,
}

impl State {
    /// Creates an empty state for a mode.
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            matches: BTreeMap::new(),
        }
    }

    /// Loads the state file, or starts fresh when it does not exist.
    ///
    /// When the persisted mode differs from `mode`, all triggers are reset so
    /// the new mode can run them. Callers must save the returned state to
    /// persist that reset.
    pub fn load(path: &Path, mode: Mode) -> Result<Self, StateError> {
        match fs::read_to_string(path) {
            Ok(contents) => {
                let mut state: Self = serde_json::from_str(&contents)?;
                state.apply_mode(mode);
                Ok(state)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::new(mode)),
            Err(error) => Err(StateError::Io(error)),
        }
    }

    /// Writes the state atomically through a temporary file and a rename.
    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        let file_name = path.file_name().ok_or(StateError::InvalidPath)?;

        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }

        let mut temp_name = file_name.to_os_string();
        temp_name.push(".tmp");
        let temp_path = path.with_file_name(temp_name);

        let contents = serde_json::to_vec_pretty(self)?;
        let result = fs::write(&temp_path, contents).and_then(|()| fs::rename(&temp_path, path));
        if let Err(error) = result {
            let _ = fs::remove_file(&temp_path);
            return Err(StateError::Io(error));
        }
        Ok(())
    }

    /// Applies the current mode, resetting all triggers when it changes.
    ///
    /// Returns whether a reset happened.
    pub fn apply_mode(&mut self, mode: Mode) -> bool {
        if self.mode == mode {
            return false;
        }

        self.mode = mode;
        for state in self.matches.values_mut() {
            state.triggers.clear();
        }
        true
    }

    /// Removes matches whose kickoff is more than [`PURGE_AFTER_DAYS`] old.
    pub fn purge(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::days(PURGE_AFTER_DAYS);
        self.matches.retain(|_, state| state.kickoff >= cutoff);
    }
}

/// Persisted state for one match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchState {
    /// Kickoff time in UTC.
    pub kickoff: DateTime<Utc>,
    /// Triggers already executed, with their execution time.
    #[serde(default)]
    pub triggers: BTreeMap<Trigger, DateTime<Utc>>,
    /// Last score successfully written to MPP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<String>,
    /// Time of the last successful write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_at: Option<DateTime<Utc>>,
}

impl MatchState {
    /// Creates state for a match that has not run any trigger.
    pub fn new(kickoff: DateTime<Utc>) -> Self {
        Self {
            kickoff,
            triggers: BTreeMap::new(),
            score: None,
            written_at: None,
        }
    }

    /// Returns whether a trigger has already run.
    pub fn is_trigger_done(&self, trigger: Trigger) -> bool {
        self.triggers.contains_key(&trigger)
    }

    /// Records a trigger execution.
    ///
    /// Returns `false` without changing the timestamp when the trigger has
    /// already run, so a completed trigger is never replayed.
    pub fn record_trigger(&mut self, trigger: Trigger, at: DateTime<Utc>) -> bool {
        if self.triggers.contains_key(&trigger) {
            return false;
        }

        self.triggers.insert(trigger, at);
        true
    }

    /// Records a successful write.
    pub fn record_write(&mut self, score: Score, at: DateTime<Utc>) {
        self.score = Some(score.to_string());
        self.written_at = Some(at);
    }

    /// Returns the last score written to MPP.
    pub fn written_score(&self) -> Option<&str> {
        self.score.as_deref()
    }
}

/// Errors raised while loading or saving the state.
#[derive(Debug, Error)]
pub enum StateError {
    /// The state file could not be read or written.
    #[error("state I/O error: {0}")]
    Io(#[from] io::Error),
    /// The state file is not valid JSON for this schema.
    #[error("state JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// The state path has no file name component.
    #[error("state path has no file name")]
    InvalidPath,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::model::Score;

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
        assert_eq!(saved.written_at, Some(at(19, 7)));
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
    fn purge_removes_only_matches_older_than_three_days() {
        let now = at(19, 12);
        let mut state = State::new(Mode::Dry);
        for (id, kickoff) in [
            ("old", now - Duration::days(4)),
            ("boundary", now - Duration::days(3)),
            ("recent", now - Duration::days(1)),
        ] {
            state
                .matches
                .insert(id.to_owned(), MatchState::new(kickoff));
        }

        state.purge(now);

        assert!(!state.matches.contains_key("old"));
        assert!(state.matches.contains_key("boundary"));
        assert!(state.matches.contains_key("recent"));
    }

    #[test]
    fn triggers_are_recorded_once() {
        let mut state = MatchState::new(at(23, 18));

        assert!(state.record_trigger(Trigger::Publish, at(19, 7)));
        assert!(!state.record_trigger(Trigger::Publish, at(19, 8)));
        assert_eq!(state.triggers.get(&Trigger::Publish), Some(&at(19, 7)));
    }

    #[test]
    fn trigger_names_and_score_match_the_spec_schema() {
        let mut state = State::new(Mode::Write);
        let mut match_state = match_state(at(23, 18));
        match_state.record_trigger(Trigger::T60, at(23, 17));
        match_state.record_trigger(Trigger::T5, at(23, 18));
        state.matches.insert("match-1".to_owned(), match_state);

        let json = serde_json::to_value(&state).expect("serialize state");
        let match_json = &json["matches"]["match-1"];
        let triggers = match_json["triggers"].as_object().expect("triggers object");

        assert_eq!(json["mode"], "write");
        assert_eq!(match_json["score"], "1-1");
        assert!(triggers.contains_key("publish"));
        assert!(triggers.contains_key("t60"));
        assert!(triggers.contains_key("t5"));
    }

    #[test]
    fn spec_state_json_is_accepted() {
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

        let state: State = serde_json::from_str(json).expect("parse spec state");
        let saved = &state.matches["mpp_championship_match_2640624"];

        assert_eq!(state.mode, Mode::Write);
        assert_eq!(
            saved.kickoff,
            Utc.with_ymd_and_hms(2026, 10, 23, 18, 45, 0)
                .single()
                .expect("valid kickoff")
        );
        assert_eq!(saved.written_score(), Some("1-1"));
        assert_eq!(saved.written_at, Some(at(19, 5) + Duration::seconds(1)));
        assert_eq!(saved.triggers.len(), 3);
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
        assert_eq!(saved.score, None);
        assert_eq!(saved.written_at, None);
    }
}
