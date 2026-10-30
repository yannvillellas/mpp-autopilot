//! Persistent state for match triggers, submitted scores, the season ledger
//! and the Elo cache.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::elo::EloRatings;
use crate::model::{Lambdas, Odds, Score};

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

/// Origin of a forecast written to MPP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForecastOrigin {
    /// Market odds.
    Market,
    /// Fresh Elo ratings.
    Elo,
    /// Elo ratings reused from the cache.
    EloCached,
}

/// The forecast written for one match, with the inputs needed to score it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastRecord {
    /// Exact score written to MPP.
    pub pick: Score,
    /// MPP quotations used by the model at write time.
    pub odds: Odds,
    /// Prediction source.
    pub source: ForecastOrigin,
    /// Observation time of the market odds or Elo ratings.
    pub observed_at: DateTime<Utc>,
    /// Expected goals used by the model.
    pub lambdas: Lambdas,
    /// Most probable exact score at write time.
    pub modal: Score,
    /// Probability of [`Self::modal`].
    pub modal_probability: f64,
}

/// MPP points breakdown for one scored match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointsRecord {
    /// Base points for the correct outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<i64>,
    /// Points for an exact score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<i64>,
    /// Extra points awarded by MPP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<i64>,
    /// Additional bonus points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bonus: Option<i64>,
    /// Total points reported by MPP.
    pub total: i64,
    /// Rarity label, when provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rarity_level: Option<String>,
}

/// One match in the season ledger, scored or still waiting for a result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeasonMatch {
    /// MPP match id.
    pub match_id: String,
    /// Kickoff in UTC.
    pub kickoff: DateTime<Utc>,
    /// Season start year.
    pub season: u16,
    /// Home club display name.
    pub home: String,
    /// Away club display name.
    pub away: String,
    /// Forecast exact score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick: Option<Score>,
    /// Final exact score, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Score>,
    /// MPP points, once the match has been scored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<PointsRecord>,
    /// Points computed from the forecast quotation and the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computed_points: Option<f64>,
    /// Points the modal score would have earned with the same quotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modal_points: Option<f64>,
    /// Forecast source, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ForecastOrigin>,
    /// Quotations used by the model at write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub odds: Option<Odds>,
    /// Most probable score at write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modal: Option<Score>,
    /// MPP match status, such as `postponed` or `decided`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Time this ledger row was last observed.
    pub observed_at: DateTime<Utc>,
}

impl SeasonMatch {
    /// Returns the season start year for a kickoff.
    ///
    /// Ligue 1 seasons start in the second half of the calendar year.
    pub fn season_from_kickoff(kickoff: DateTime<Utc>) -> u16 {
        let year = if kickoff.month() >= 7 {
            kickoff.year()
        } else {
            kickoff.year() - 1
        };
        u16::try_from(year).unwrap_or_default()
    }

    /// Merges a newer observation into this ledger row.
    ///
    /// The first observation keeps the write-time forecast data. Later
    /// observations fill gaps and update the result, MPP points and status.
    /// Returns whether anything changed.
    pub fn merge(&mut self, newer: Self) -> bool {
        let mut changed = false;

        if self.pick != newer.pick {
            self.pick = newer.pick;
            self.source = newer.source;
            self.odds = newer.odds;
            self.modal = newer.modal;
            self.computed_points = newer.computed_points;
            self.modal_points = newer.modal_points;
            changed = true;
        } else {
            if self.source.is_none() && newer.source.is_some() {
                self.source = newer.source;
                changed = true;
            }
            if self.odds.is_none() && newer.odds.is_some() {
                self.odds = newer.odds;
                changed = true;
            }
            if self.modal.is_none() && newer.modal.is_some() {
                self.modal = newer.modal;
                changed = true;
            }
            if self.computed_points.is_none() && newer.computed_points.is_some() {
                self.computed_points = newer.computed_points;
                changed = true;
            }
            if self.modal_points.is_none() && newer.modal_points.is_some() {
                self.modal_points = newer.modal_points;
                changed = true;
            }
        }

        if newer.result.is_some() && self.result != newer.result {
            self.result = newer.result;
            self.computed_points = newer.computed_points;
            self.modal_points = newer.modal_points;
            changed = true;
        }

        if newer.points.is_some() && self.points != newer.points {
            self.points = newer.points;
            changed = true;
        }

        if newer.status.is_some() && self.status != newer.status {
            self.status = newer.status;
            changed = true;
        }

        if changed {
            self.observed_at = newer.observed_at;
        }

        changed
    }
}

/// Persisted state shared by all matches, the season ledger and the Elo cache.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Last execution mode.
    pub mode: Mode,
    /// Match state, keyed by MPP match id.
    pub matches: BTreeMap<String, MatchState>,
    /// Matches kept for the season total, keyed by MPP match id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub season_matches: BTreeMap<String, SeasonMatch>,
    /// Authenticated MPP user id, cached after the first fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
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
            season_matches: BTreeMap::new(),
            user_id: None,
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
        let mut state = Self::load_unmodified(path)?;
        state.apply_mode(mode);
        Ok(state)
    }

    /// Loads the state file without applying the current mode.
    ///
    /// Read-only reports use this so inspecting the ledger never resets
    /// triggers when the configured mode differs from the persisted one.
    pub fn load_unmodified(path: &Path) -> Result<Self, StateError> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(serde_json::from_str(&contents)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::new(Mode::Dry)),
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

    /// Inserts or merges one season-ledger row.
    ///
    /// Returns whether the stored row changed.
    pub fn record_season_match(&mut self, scored: SeasonMatch) -> bool {
        match self.season_matches.get_mut(&scored.match_id) {
            Some(existing) => existing.merge(scored),
            None => {
                self.season_matches.insert(scored.match_id.clone(), scored);
                true
            }
        }
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Forecast written to MPP, with the inputs needed to score it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast: Option<ForecastRecord>,
    /// Last observed MPP status for the match, such as `postponed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_status: Option<String>,
}

impl MatchState {
    /// Creates state for a match that has not run any trigger.
    pub fn new(kickoff: DateTime<Utc>) -> Self {
        Self {
            kickoff,
            triggers: BTreeMap::new(),
            last_written_score: None,
            last_written_at: None,
            forecast: None,
            result_status: None,
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
