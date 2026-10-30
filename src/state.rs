//! Persistent state for match triggers, submitted scores and the Elo cache.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::elo::EloRatings;
use crate::model::Score;

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
    /// One minute before kickoff.
    #[serde(rename = "t1")]
    T1,
}

impl Trigger {
    /// All triggers, in execution order.
    pub const ALL: [Self; 4] = [Self::Publish, Self::T60, Self::T5, Self::T1];
}

/// Persisted state shared by all matches and the Elo cache.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Last execution mode.
    pub mode: Mode,
    /// Match state, keyed by MPP match id.
    pub matches: BTreeMap<String, MatchState>,
    /// Last successfully fetched Elo ratings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elo: Option<CachedElo>,
}

impl State {
    /// Creates an empty state for a mode.
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            matches: BTreeMap::new(),
            elo: None,
        }
    }

    /// Records the ratings from a successful source fetch.
    pub fn record_elo(&mut self, mut ratings: EloRatings, at: DateTime<Utc>) {
        let applied = std::mem::take(&mut ratings.applied);
        let backfilled_through = ratings.backfilled_through.take();
        self.elo = Some(CachedElo {
            fetched_at: at,
            ratings,
            applied,
            backfilled_through,
        });
    }

    /// Returns the cached ratings with their incremental-update progress.
    ///
    /// Returns empty ratings when no successful fetch has been recorded.
    pub fn cached_elo(&self) -> EloRatings {
        self.elo
            .as_ref()
            .map(CachedElo::restore_ratings)
            .unwrap_or_default()
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

    /// Removes matches that the current MPP snapshot no longer lists.
    ///
    /// A finished match is kept for as long as the snapshot lists it, so its
    /// write record survives the daily pass instead of being recreated blank.
    pub fn purge_missing(&mut self, live: &BTreeSet<String>) {
        self.matches.retain(|match_id, _| live.contains(match_id));
    }
}

/// Last successfully fetched Elo ratings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedElo {
    /// Time of the fetch that produced these ratings.
    pub fetched_at: DateTime<Utc>,
    /// Ratings indexed by normalized club name.
    pub ratings: EloRatings,
    /// Matches already applied to the ratings, keyed by season, date and clubs.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    applied: BTreeSet<String>,
    /// Last season whose CSV was downloaded, as a start year.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backfilled_through: Option<u16>,
}

impl CachedElo {
    /// Restores the ratings together with the progress needed for an
    /// incremental update.
    pub fn restore_ratings(&self) -> EloRatings {
        let mut ratings = self.ratings.clone();
        ratings.applied = self.applied.clone();
        ratings.backfilled_through = self.backfilled_through;
        ratings
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
    ///
    /// Audit trail only: idempotence compares against the forecast read back
    /// from MPP, never against this field.
    #[serde(default, alias = "score", skip_serializing_if = "Option::is_none")]
    pub last_written_score: Option<String>,
    /// Time of the last successful write.
    #[serde(default, alias = "written_at", skip_serializing_if = "Option::is_none")]
    pub last_written_at: Option<DateTime<Utc>>,
}

impl MatchState {
    /// Creates state for a match that has not run any trigger.
    pub fn new(kickoff: DateTime<Utc>) -> Self {
        Self {
            kickoff,
            triggers: BTreeMap::new(),
            last_written_score: None,
            last_written_at: None,
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
        self.last_written_score = Some(score.to_string());
        self.last_written_at = Some(at);
    }

    /// Returns the last score written to MPP.
    pub fn written_score(&self) -> Option<&str> {
        self.last_written_score.as_deref()
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
