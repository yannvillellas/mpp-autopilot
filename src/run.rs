//! Orchestration of the daily fetch, match triggers and MPP writes.
//!
//! Idempotence reads the forecast back from MPP before every write: a trigger
//! writes only when the remote score differs from the model pick, so a manual
//! edit in the app is overwritten at the next trigger and a crash between the
//! PATCH and the state write is corrected instead of replayed. When the
//! forecast cannot be read, the trigger writes unconditionally: a wrong or
//! missing score is worse than a redundant PATCH.
//! The persistent state remains the scheduler memory: which triggers already
//! ran, the kickoff of each match, and an audit trail of what the bot wrote.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use thiserror::Error;
use tracing::{info, warn};

use crate::elo::{EloError, EloSite};
use crate::model::{self, Prediction};
use crate::mpp::{Fixture, MppApi, MppError, Snapshot};
use crate::scheduler;
use crate::state::{MatchState, Mode, State, StateError, Trigger};
use crate::teams;

/// Counters for one orchestrator pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RunReport {
    /// Matches returned by the daily fetch.
    pub matches: usize,
    /// Triggers executed or marked without a write.
    pub triggers: usize,
    /// Predictions actually written to MPP.
    pub writes: usize,
    /// Triggers skipped because MPP already holds the model pick.
    pub unchanged: usize,
    /// Triggers in dry-run mode, counted before any forecast comparison:
    /// dry-run does not read MPP and therefore cannot detect a pick already
    /// registered there.
    pub dry_writes: usize,
    /// Triggers skipped for missing data, model errors, a passed kickoff or
    /// a non-transient write failure.
    pub skipped: usize,
}

impl RunReport {
    fn merge(&mut self, other: Self) {
        self.matches += other.matches;
        self.triggers += other.triggers;
        self.writes += other.writes;
        self.unchanged += other.unchanged;
        self.dry_writes += other.dry_writes;
        self.skipped += other.skipped;
    }
}

/// Errors raised by the orchestrator.
#[derive(Debug, Error)]
pub enum RunError {
    /// MPP request or parsing failure.
    #[error("MPP error: {0}")]
    Mpp(#[from] MppError),
    /// ClubElo request or parsing failure.
    #[error("ClubElo error: {0}")]
    Elo(#[from] EloError),
    /// State persistence failure.
    #[error("state error: {0}")]
    State(#[from] StateError),
    /// A trigger pass ran before any daily snapshot was loaded.
    #[error("daily snapshot required before a trigger pass")]
    MissingSnapshot,
}

impl RunError {
    /// Returns whether the operation may succeed later.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Mpp(error) => error.is_transient(),
            Self::Elo(error) => error.is_transient(),
            Self::State(_) | Self::MissingSnapshot => false,
        }
    }
}

/// Runs daily fetches and match triggers against injected clients.
pub struct Orchestrator<'a, M: MppApi, E: EloSite> {
    mpp: &'a M,
    elo: &'a E,
    mode: Mode,
    state: State,
    state_path: PathBuf,
    fixtures: HashMap<String, Fixture>,
    snapshot_loaded: bool,
}

impl<'a, M: MppApi, E: EloSite> Orchestrator<'a, M, E> {
    /// Builds an orchestrator from loaded state and clients.
    ///
    /// The state mode is aligned with `mode`; a mode change resets triggers,
    /// as required by the persistent-state contract.
    pub fn new(mpp: &'a M, elo: &'a E, mode: Mode, mut state: State, state_path: PathBuf) -> Self {
        state.apply_mode(mode);

        Self {
            mpp,
            elo,
            mode,
            state,
            state_path,
            fixtures: HashMap::new(),
            snapshot_loaded: false,
        }
    }

    /// Current persistent state.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Returns whether a daily snapshot has been loaded.
    pub fn has_snapshot(&self) -> bool {
        self.snapshot_loaded
    }

    /// Fetches the daily MPP snapshot, updates the state and persists it.
    pub async fn daily_pass(&mut self, now: DateTime<Utc>) -> Result<RunReport, RunError> {
        let Snapshot { matches, .. } = self.mpp.fetch_snapshot().await?;
        let count = matches.len();

        self.state.purge(now);

        let mut fixtures = HashMap::with_capacity(count);
        for fixture in matches {
            if let Some(existing) = self.state.matches.get_mut(&fixture.id) {
                if existing.kickoff != fixture.kickoff {
                    warn!(
                        match_id = %fixture.id,
                        old = %existing.kickoff,
                        new = %fixture.kickoff,
                        "kickoff changed; resetting pre-match triggers"
                    );
                    existing.kickoff = fixture.kickoff;
                    existing.triggers.remove(&Trigger::T60);
                    existing.triggers.remove(&Trigger::T5);
                }
            } else {
                self.state
                    .matches
                    .insert(fixture.id.clone(), MatchState::new(fixture.kickoff));
            }

            fixtures.insert(fixture.id.clone(), fixture);
        }

        self.fixtures = fixtures;
        self.state.save(&self.state_path)?;
        self.snapshot_loaded = true;
        info!(matches = count, "daily MPP fetch complete");

        Ok(RunReport {
            matches: count,
            ..RunReport::default()
        })
    }

    /// Executes every trigger that is due at `now`.
    pub async fn trigger_pass(&mut self, now: DateTime<Utc>) -> Result<RunReport, RunError> {
        let mut due: Vec<(String, Vec<Trigger>)> = self
            .state
            .matches
            .iter()
            .map(|(id, state)| (id.clone(), scheduler::due_triggers(state, now)))
            .filter(|(_, triggers)| !triggers.is_empty())
            .collect();

        if due.is_empty() {
            return Ok(RunReport::default());
        }

        if !self.snapshot_loaded {
            return Err(RunError::MissingSnapshot);
        }

        due.sort_by(|(left, _), (right, _)| {
            self.state.matches[left]
                .kickoff
                .cmp(&self.state.matches[right].kickoff)
                .then_with(|| left.cmp(right))
        });

        let mut report = RunReport::default();
        let mut future = Vec::new();

        for (match_id, triggers) in due {
            for trigger in triggers {
                if self.state.matches[&match_id].kickoff <= now {
                    self.mark_trigger(&match_id, trigger, now)?;
                    report.triggers += 1;
                    report.skipped += 1;
                    info!(
                        match_id = %match_id,
                        trigger = ?trigger,
                        "kickoff already passed; trigger marked without write"
                    );
                } else {
                    future.push((match_id.clone(), trigger));
                }
            }
        }

        if future.is_empty() {
            return Ok(report);
        }

        let mut ready = Vec::with_capacity(future.len());
        for (match_id, trigger) in future {
            let Some(fixture) = self.fixtures.get(&match_id).cloned() else {
                warn!(
                    match_id = %match_id,
                    trigger = ?trigger,
                    kickoff = %self.state.matches[&match_id].kickoff,
                    "fixture missing from daily snapshot; trigger marked without write"
                );
                self.mark_trigger(&match_id, trigger, now)?;
                report.triggers += 1;
                report.skipped += 1;
                continue;
            };
            ready.push((match_id, trigger, fixture));
        }

        if ready.is_empty() {
            return Ok(report);
        }

        let ratings = self.elo.fetch_ratings().await?;
        let mut first_error: Option<RunError> = None;

        for (match_id, trigger, fixture) in ready {
            let Some(home_canonical) = teams::canonical_name(&fixture.home.name) else {
                warn!(
                    match_id = %match_id,
                    trigger = ?trigger,
                    club = %fixture.home.name,
                    "unknown home club; trigger marked without write"
                );
                self.mark_trigger(&match_id, trigger, now)?;
                report.triggers += 1;
                report.skipped += 1;
                continue;
            };

            let Some(away_canonical) = teams::canonical_name(&fixture.away.name) else {
                warn!(
                    match_id = %match_id,
                    trigger = ?trigger,
                    club = %fixture.away.name,
                    "unknown away club; trigger marked without write"
                );
                self.mark_trigger(&match_id, trigger, now)?;
                report.triggers += 1;
                report.skipped += 1;
                continue;
            };

            let (Some(home_elo), Some(away_elo)) = (
                ratings.rating(home_canonical),
                ratings.rating(away_canonical),
            ) else {
                warn!(
                    match_id = %match_id,
                    trigger = ?trigger,
                    home = %fixture.home.name,
                    away = %fixture.away.name,
                    "missing ClubElo rating; trigger marked without write"
                );
                self.mark_trigger(&match_id, trigger, now)?;
                report.triggers += 1;
                report.skipped += 1;
                continue;
            };

            let prediction = match model::predict(home_elo, away_elo, fixture.odds) {
                Ok(prediction) => prediction,
                Err(error) => {
                    warn!(
                        match_id = %match_id,
                        trigger = ?trigger,
                        error = %error,
                        "model could not compute a prediction; trigger marked without write"
                    );
                    self.mark_trigger(&match_id, trigger, now)?;
                    report.triggers += 1;
                    report.skipped += 1;
                    continue;
                }
            };

            if self.mode == Mode::Dry {
                log_trigger(
                    &fixture,
                    trigger,
                    home_elo,
                    away_elo,
                    &prediction,
                    "dry_run",
                );
                self.mark_trigger(&match_id, trigger, now)?;
                report.triggers += 1;
                report.dry_writes += 1;
                continue;
            }

            let (remote, remote_known) = match self.mpp.fetch_forecast(&match_id).await {
                Ok(forecast) => (forecast, true),
                Err(error) => {
                    warn!(
                        match_id = %match_id,
                        trigger = ?trigger,
                        error = %error,
                        "cannot read the MPP forecast; writing without the idempotence check"
                    );
                    (None, false)
                }
            };

            if remote == Some(prediction.pick) {
                log_trigger(
                    &fixture,
                    trigger,
                    home_elo,
                    away_elo,
                    &prediction,
                    "unchanged",
                );
                self.mark_trigger(&match_id, trigger, now)?;
                report.triggers += 1;
                report.unchanged += 1;
                continue;
            }

            let last_written = self.state.matches[&match_id].written_score();
            let remote_text = remote.map(|score| score.to_string());
            let reason = match (remote_known, remote_text.as_deref()) {
                // The read failed: already warned, nothing was observed.
                (false, _) => None,
                (true, None) => Some("no forecast registered"),
                (true, Some(remote)) if last_written != Some(remote) => Some("manual edit"),
                (true, Some(_)) => None,
            };
            if let Some(reason) = reason {
                info!(
                    match_id = %match_id,
                    trigger = ?trigger,
                    reason,
                    last_written = ?last_written,
                    remote = ?remote_text,
                    pick = %prediction.pick,
                    "overwriting the registered forecast"
                );
            }

            match self.mpp.submit_forecast(&match_id, prediction.pick).await {
                Ok(()) => {
                    let match_state = self
                        .state
                        .matches
                        .get_mut(&match_id)
                        .expect("match state exists while processing its trigger");
                    match_state.record_write(prediction.pick, now);
                    match_state.record_trigger(trigger, now);
                    log_trigger(
                        &fixture,
                        trigger,
                        home_elo,
                        away_elo,
                        &prediction,
                        "written",
                    );
                    self.state.save(&self.state_path)?;
                    report.triggers += 1;
                    report.writes += 1;
                }
                Err(error) if error.is_transient() => {
                    log_trigger(
                        &fixture,
                        trigger,
                        home_elo,
                        away_elo,
                        &prediction,
                        "write_failed",
                    );
                    warn!(
                        match_id = %match_id,
                        trigger = ?trigger,
                        error = %error,
                        "transient MPP write failure; trigger left pending"
                    );
                    match error {
                        MppError::Network(_) | MppError::Unauthorized => {
                            return Err(error.into());
                        }
                        other => {
                            if first_error.is_none() {
                                first_error = Some(other.into());
                            }
                        }
                    }
                }
                Err(error) => {
                    log_trigger(
                        &fixture,
                        trigger,
                        home_elo,
                        away_elo,
                        &prediction,
                        "write_failed",
                    );
                    warn!(
                        match_id = %match_id,
                        trigger = ?trigger,
                        error = %error,
                        "non-transient MPP write failure; trigger marked without write"
                    );
                    self.mark_trigger(&match_id, trigger, now)?;
                    report.triggers += 1;
                    report.skipped += 1;
                }
            }
        }

        if let Some(error) = first_error {
            return Err(error);
        }

        Ok(report)
    }

    /// Runs one daily fetch followed by every trigger due at `now`.
    pub async fn execute_once(&mut self, now: DateTime<Utc>) -> Result<RunReport, RunError> {
        let mut report = self.daily_pass(now).await?;
        let triggers = self.trigger_pass(now).await?;
        report.merge(triggers);
        Ok(report)
    }

    fn mark_trigger(
        &mut self,
        match_id: &str,
        trigger: Trigger,
        now: DateTime<Utc>,
    ) -> Result<(), RunError> {
        if let Some(match_state) = self.state.matches.get_mut(match_id) {
            match_state.record_trigger(trigger, now);
        }
        self.state.save(&self.state_path)?;
        Ok(())
    }
}

fn log_trigger(
    fixture: &Fixture,
    trigger: Trigger,
    home_elo: f64,
    away_elo: f64,
    prediction: &Prediction,
    status: &'static str,
) {
    info!(
        match_id = %fixture.id,
        home = %fixture.home.name,
        away = %fixture.away.name,
        kickoff = %fixture.kickoff,
        trigger = ?trigger,
        home_elo,
        away_elo,
        odds_home = fixture.odds.home,
        odds_draw = fixture.odds.draw,
        odds_away = fixture.odds.away,
        lambda_home = prediction.lambdas.home,
        lambda_away = prediction.lambdas.away,
        p_home = prediction.outcomes.home,
        p_draw = prediction.outcomes.draw,
        p_away = prediction.outcomes.away,
        pick = %prediction.pick,
        pick_ev = prediction.pick_ev,
        pick_probability = prediction.pick_probability,
        recommendation = ?prediction.recommendation,
        recommendation_ev = prediction.recommendation_ev,
        status,
        "MPP trigger"
    );
}
