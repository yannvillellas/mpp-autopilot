//! Orchestration of the daily fetch, match triggers and MPP writes.
//!
//! Market odds are the primary prediction source. A fixture without a usable
//! market quote falls back to Elo ratings, first fetched and then cached.
//!
//! Idempotence reads the forecast back from MPP before every write: a trigger
//! writes only when the remote score differs from the model pick, so a manual
//! edit in the app is overwritten at the next trigger and a crash between the
//! PATCH and the state write is corrected instead of replayed. When the
//! forecast cannot be read, the trigger writes unconditionally: a wrong or
//! missing score is worse than a redundant PATCH.
//! The persistent state remains the scheduler memory: which triggers already
//! ran, the kickoff of each match, and an audit trail of what the bot wrote.
//! It also keeps the last successful Elo ratings, so a fetch failure does not
//! drop the whole pass; ratings reused from that cache are logged with their
//! age.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use thiserror::Error;
use tracing::{info, warn};

use crate::elo::{EloError, EloRatings, EloSite};
use crate::model::{self, Prediction, Score};
use crate::mpp::{
    Club, Fixture, ForecastHistory, HistoricalMatch, HistoryPoints, MppApi, MppError, Snapshot,
};
use crate::odds::{MarketForecast, NoOdds, OddsSite};
use crate::scheduler;
use crate::state::{
    ForecastOrigin, ForecastRecord, MatchState, Mode, PointsRecord, SeasonMatch, State, StateError,
    Trigger,
};
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
    /// Matches whose pending pre-match triggers were closed because kickoff
    /// had already passed. Counted once per match, not once per trigger.
    pub finished: usize,
    /// Season-ledger rows inserted or updated from the forecast history.
    pub results: usize,
    /// Triggers skipped for missing data, model errors or a non-transient
    /// write failure.
    pub skipped: usize,
}

impl RunReport {
    fn merge(&mut self, other: Self) {
        self.matches += other.matches;
        self.triggers += other.triggers;
        self.writes += other.writes;
        self.unchanged += other.unchanged;
        self.dry_writes += other.dry_writes;
        self.finished += other.finished;
        self.results += other.results;
        self.skipped += other.skipped;
    }
}

/// Errors raised by the orchestrator.
#[derive(Debug, Error)]
pub enum RunError {
    /// MPP request or parsing failure.
    #[error("MPP error: {0}")]
    Mpp(#[from] MppError),
    /// Elo ratings request or parsing failure.
    #[error("Elo ratings error: {0}")]
    Elo(#[from] EloError),
    /// State persistence failure.
    #[error("state error: {0}")]
    State(#[from] StateError),
    /// A trigger pass ran before any daily snapshot was loaded.
    #[error("daily snapshot required before a trigger pass")]
    MissingSnapshot,
    /// Cached Elo ratings do not cover a fixture, so a fresh fetch is required
    /// before the trigger can be consumed.
    #[error("cached Elo ratings are missing a club rating")]
    CachedEloIncomplete,
}

impl RunError {
    /// Returns whether the operation may succeed later.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Mpp(error) => error.is_transient(),
            Self::Elo(error) => error.is_transient(),
            Self::CachedEloIncomplete => true,
            Self::State(_) | Self::MissingSnapshot => false,
        }
    }
}

/// Runs daily fetches and match triggers against injected clients.
///
/// A market source is optional so the Elo-only path stays testable. Production
/// wiring always adds one with [`Self::with_market`].
pub struct Orchestrator<'a, M: MppApi, E: EloSite, O: OddsSite = NoOdds> {
    mpp: &'a M,
    elo: &'a E,
    market: Option<&'a O>,
    mode: Mode,
    state: State,
    state_path: PathBuf,
    fixtures: HashMap<String, Fixture>,
    snapshot_loaded: bool,
}

impl<'a, M: MppApi, E: EloSite> Orchestrator<'a, M, E, NoOdds> {
    /// Builds an Elo-only orchestrator from loaded state and clients.
    ///
    /// The state mode is aligned with `mode`; a mode change resets triggers,
    /// as required by the persistent-state contract.
    pub fn new(mpp: &'a M, elo: &'a E, mode: Mode, mut state: State, state_path: PathBuf) -> Self {
        state.apply_mode(mode);

        Self {
            mpp,
            elo,
            market: None,
            mode,
            state,
            state_path,
            fixtures: HashMap::new(),
            snapshot_loaded: false,
        }
    }

    /// Adds the market odds source used before the Elo fallback.
    pub fn with_market<O: OddsSite>(self, market: &'a O) -> Orchestrator<'a, M, E, O> {
        Orchestrator {
            mpp: self.mpp,
            elo: self.elo,
            market: Some(market),
            mode: self.mode,
            state: self.state,
            state_path: self.state_path,
            fixtures: self.fixtures,
            snapshot_loaded: self.snapshot_loaded,
        }
    }
}

impl<'a, M: MppApi, E: EloSite, O: OddsSite> Orchestrator<'a, M, E, O> {
    /// Current persistent state.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Returns whether a daily snapshot has been loaded.
    pub fn has_snapshot(&self) -> bool {
        self.snapshot_loaded
    }

    /// Fetches the daily MPP snapshot, updates the state and persists it.
    ///
    /// The forecast history is read once per pass after the snapshot. A
    /// history failure is logged and never fails the pass: results are
    /// read-only and must not block forecasts.
    pub async fn daily_pass(&mut self, now: DateTime<Utc>) -> Result<RunReport, RunError> {
        let Snapshot { clubs, matches } = self.mpp.fetch_snapshot().await?;
        let count = matches.len();
        let live: BTreeSet<String> = matches.iter().map(|fixture| fixture.id.clone()).collect();

        self.state.purge_missing(&live);

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
                    existing.triggers.remove(&Trigger::T1);
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

        let mut report = RunReport {
            matches: count,
            ..RunReport::default()
        };
        let mut changed = false;
        let user_id = match self.state.user_id.clone() {
            Some(user_id) => Some(user_id),
            None => match self.mpp.fetch_user_id().await {
                Ok(user_id) => {
                    self.state.user_id = Some(user_id.clone());
                    changed = true;
                    Some(user_id)
                }
                Err(error) => {
                    warn!(
                        error = %error,
                        "cannot read the MPP user id; results are left for a later pass"
                    );
                    None
                }
            },
        };

        if let Some(user_id) = user_id {
            match self.mpp.fetch_forecast_history(&user_id, None).await {
                Ok(history) => {
                    let (updates, status_changed) = self.merge_history(&clubs, &history, now);
                    report.results = updates;
                    changed |= updates > 0 || status_changed;
                }
                Err(error) => {
                    warn!(
                        error = %error,
                        "forecast history unavailable; results are left for a later pass"
                    );
                }
            }
        }

        if changed {
            self.state.save(&self.state_path)?;
        }

        info!(
            matches = count,
            results = report.results,
            at = %now,
            "daily MPP fetch complete"
        );

        Ok(report)
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
            let has_result = self
                .state
                .season_matches
                .get(&match_id)
                .is_some_and(|season_match| season_match.result.is_some());
            if self.state.matches[&match_id].kickoff <= now || has_result {
                self.close_finished_match(&match_id, &triggers, now)?;
                report.finished += 1;
                continue;
            }

            for trigger in triggers {
                future.push((match_id.clone(), trigger));
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

        let market = match self.market {
            Some(market) => match market.fetch_forecasts().await {
                Ok(forecasts) => forecasts,
                Err(error) => {
                    warn!(
                        error = %error,
                        "market odds unavailable; falling back to Elo ratings"
                    );
                    Vec::new()
                }
            },
            None => Vec::new(),
        };

        let mut ratings_error: Option<RunError> = None;
        let ratings = match self.ratings(now).await {
            Ok(ratings) => Some(ratings),
            Err(error) => {
                warn!(
                    error = %error,
                    "Elo ratings unavailable; market odds will be used where available"
                );
                ratings_error = Some(error);
                None
            }
        };
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

            let market_prediction =
                find_market_forecast(&market, &fixture, home_canonical, away_canonical).and_then(
                    |forecast| match model::predict_from_lambdas(forecast.lambdas, fixture.odds) {
                        Ok(prediction) => Some((
                            prediction,
                            ForecastSource::Market {
                                observed_at: forecast.observed_at,
                            },
                        )),
                        Err(error) => {
                            warn!(
                                match_id = %match_id,
                                trigger = ?trigger,
                                error = %error,
                                "market odds could not produce a prediction; falling back to Elo"
                            );
                            None
                        }
                    },
                );

            let (prediction, elo, source) = if let Some((prediction, source)) = market_prediction {
                (prediction, None, source)
            } else {
                let Some((ratings, ratings_source)) = ratings.as_ref() else {
                    warn!(
                        match_id = %match_id,
                        trigger = ?trigger,
                        home = %fixture.home.name,
                        away = %fixture.away.name,
                        "no market odds and no Elo ratings; trigger left pending"
                    );
                    if first_error.is_none() {
                        first_error = ratings_error.take();
                    }
                    continue;
                };

                let (Some(home_elo), Some(away_elo)) = (
                    ratings.rating(home_canonical),
                    ratings.rating(away_canonical),
                ) else {
                    if ratings_source.is_cached() {
                        warn!(
                            match_id = %match_id,
                            trigger = ?trigger,
                            home = %fixture.home.name,
                            away = %fixture.away.name,
                            "missing Elo rating in the cached snapshot; trigger left pending"
                        );
                        if first_error.is_none() {
                            first_error = Some(RunError::CachedEloIncomplete);
                        }
                        continue;
                    }

                    warn!(
                        match_id = %match_id,
                        trigger = ?trigger,
                        home = %fixture.home.name,
                        away = %fixture.away.name,
                        "missing Elo rating; trigger marked without write"
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

                (
                    prediction,
                    Some(MatchElo {
                        home: home_elo,
                        away: away_elo,
                    }),
                    ForecastSource::Elo {
                        observed_at: ratings_source.observed_at(now),
                        ratings: *ratings_source,
                    },
                )
            };

            if self.mode == Mode::Dry {
                log_trigger(
                    &fixture,
                    trigger,
                    elo,
                    &prediction,
                    RemoteForecast::NotRead,
                    source,
                    "dry_run",
                );
                self.mark_trigger(&match_id, trigger, now)?;
                report.triggers += 1;
                report.dry_writes += 1;
                continue;
            }

            let remote = match self.mpp.fetch_forecast(&match_id).await {
                Ok(Some(score)) => RemoteForecast::Registered(score),
                Ok(None) => RemoteForecast::Absent,
                Err(error) => {
                    warn!(
                        match_id = %match_id,
                        trigger = ?trigger,
                        error = %error,
                        "cannot read the MPP forecast; writing without the idempotence check"
                    );
                    RemoteForecast::Unreadable
                }
            };

            if remote.score() == Some(prediction.pick) {
                if let Some(match_state) = self.state.matches.get_mut(&match_id) {
                    if match_state.forecast.is_none() {
                        match_state.forecast = Some(forecast_record(&prediction, &fixture, source));
                    }
                }
                log_trigger(
                    &fixture,
                    trigger,
                    elo,
                    &prediction,
                    remote,
                    source,
                    "unchanged",
                );
                self.mark_trigger(&match_id, trigger, now)?;
                report.triggers += 1;
                report.unchanged += 1;
                continue;
            }

            let last_written = self.state.matches[&match_id].written_score();
            let remote_text = remote.score().map(|score| score.to_string());
            let reason = match remote {
                // The read failed: already warned, nothing was observed.
                RemoteForecast::Unreadable | RemoteForecast::NotRead => None,
                RemoteForecast::Absent => Some("no forecast registered"),
                RemoteForecast::Registered(_) if last_written != remote_text.as_deref() => {
                    Some("remote forecast differs from the last write")
                }
                RemoteForecast::Registered(_) => Some("model pick changed"),
            };
            if let Some(reason) = reason {
                info!(
                    match_id = %match_id,
                    trigger = ?trigger,
                    reason,
                    last_written = ?last_written,
                    remote = %remote,
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
                    match_state.forecast = Some(forecast_record(&prediction, &fixture, source));
                    log_trigger(
                        &fixture,
                        trigger,
                        elo,
                        &prediction,
                        remote,
                        source,
                        "written",
                    );
                    self.state.save(&self.state_path)?;
                    report.triggers += 1;
                    report.writes += 1;
                }
                Err(error) if error.is_kickoff_locked() => {
                    info!(
                        match_id = %match_id,
                        trigger = ?trigger,
                        kickoff = %self.state.matches[&match_id].kickoff,
                        error = %error,
                        "kickoff passed; MPP locked the forecast; trigger marked without write"
                    );
                    self.mark_trigger(&match_id, trigger, now)?;
                    report.triggers += 1;
                    report.skipped += 1;
                }
                Err(error) if error.is_transient() => {
                    log_trigger(
                        &fixture,
                        trigger,
                        elo,
                        &prediction,
                        remote,
                        source,
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
                Err(error) if error.is_forbidden() => {
                    log_trigger(
                        &fixture,
                        trigger,
                        elo,
                        &prediction,
                        remote,
                        source,
                        "write_failed",
                    );
                    warn!(
                        match_id = %match_id,
                        trigger = ?trigger,
                        error = %error,
                        "MPP write forbidden; trigger left pending"
                    );
                    return Err(error.into());
                }
                Err(error) => {
                    log_trigger(
                        &fixture,
                        trigger,
                        elo,
                        &prediction,
                        remote,
                        source,
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

    /// Fetches the Elo ratings, reusing the last successful snapshot when the
    /// fetch fails.
    async fn ratings(
        &mut self,
        now: DateTime<Utc>,
    ) -> Result<(EloRatings, RatingsSource), RunError> {
        match self.elo.fetch_ratings().await {
            Ok(ratings) => {
                self.state.record_elo(ratings.clone(), now);
                self.state.save(&self.state_path)?;
                Ok((ratings, RatingsSource::Fresh))
            }
            Err(error) => {
                let Some(cached) = self
                    .state
                    .elo
                    .as_ref()
                    .filter(|cached| !cached.ratings.is_empty())
                else {
                    return Err(error.into());
                };
                let age_seconds = now
                    .signed_duration_since(cached.fetched_at)
                    .num_seconds()
                    .max(0);
                warn!(
                    error = %error,
                    fetched_at = %cached.fetched_at,
                    age_seconds,
                    ratings = cached.ratings.len(),
                    "Elo ratings fetch failed; using the last successful ratings"
                );
                Ok((
                    cached.ratings.clone(),
                    RatingsSource::Cached {
                        fetched_at: cached.fetched_at,
                        age_seconds,
                    },
                ))
            }
        }
    }

    /// Merges one history page into the season ledger.
    ///
    /// Returns the number of ledger rows inserted or updated, and whether a
    /// match status changed without touching the ledger.
    fn merge_history(
        &mut self,
        clubs: &HashMap<String, Club>,
        history: &ForecastHistory,
        now: DateTime<Utc>,
    ) -> (usize, bool) {
        let mut updates = 0;
        let mut status_changed = false;

        for historical in &history.matches {
            let local = self
                .state
                .matches
                .get(&historical.id)
                .and_then(|match_state| match_state.forecast.clone());

            if let Some(match_state) = self.state.matches.get_mut(&historical.id) {
                if match_state.result_status != historical.match_status {
                    match_state.result_status = historical.match_status.clone();
                    status_changed = true;
                }
            }

            let Some(row) = season_match(clubs, historical, local.as_ref(), now) else {
                continue;
            };

            verify_points(&row);

            if let Some(existing) = self.state.season_matches.get(&row.match_id) {
                if existing.result.is_some()
                    && row.result.is_some()
                    && existing.result != row.result
                {
                    warn!(
                        match_id = %row.match_id,
                        old = ?existing.result,
                        new = ?row.result,
                        "MPP result changed; updating the season ledger"
                    );
                }
            }

            if self.state.record_season_match(row) {
                updates += 1;
            }
        }

        (updates, status_changed)
    }

    /// Runs one daily fetch followed by every trigger due at `now`.
    pub async fn execute_once(&mut self, now: DateTime<Utc>) -> Result<RunReport, RunError> {
        let mut report = self.daily_pass(now).await?;
        let triggers = self.trigger_pass(now).await?;
        report.merge(triggers);
        Ok(report)
    }

    fn close_finished_match(
        &mut self,
        match_id: &str,
        triggers: &[Trigger],
        now: DateTime<Utc>,
    ) -> Result<(), RunError> {
        if let Some(match_state) = self.state.matches.get_mut(match_id) {
            for trigger in triggers {
                match_state.record_trigger(*trigger, now);
            }
        }
        self.state.save(&self.state_path)?;
        info!(
            match_id = %match_id,
            triggers = triggers.len(),
            "match already finished; pending triggers closed"
        );
        Ok(())
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

/// Outcome of reading the registered forecast back from MPP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteForecast {
    /// Dry-run: MPP was not queried.
    NotRead,
    /// The read failed; the registered score is unknown.
    Unreadable,
    /// MPP returned no forecast for the match.
    Absent,
    /// MPP holds this score.
    Registered(Score),
}

impl RemoteForecast {
    /// Returns the registered score, when MPP returned one.
    fn score(self) -> Option<Score> {
        match self {
            Self::Registered(score) => Some(score),
            Self::NotRead | Self::Unreadable | Self::Absent => None,
        }
    }
}

impl fmt::Display for RemoteForecast {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRead => f.write_str("not_read"),
            Self::Unreadable => f.write_str("unreadable"),
            Self::Absent => f.write_str("absent"),
            Self::Registered(score) => write!(f, "{score}"),
        }
    }
}

/// Elo ratings used for one fixture.
#[derive(Debug, Clone, Copy)]
struct MatchElo {
    home: f64,
    away: f64,
}

/// Maximum kickoff difference accepted when matching an odds event.
const MARKET_KICKOFF_TOLERANCE_SECS: i64 = 6 * 60 * 60;

/// Finds the market forecast of a fixture by canonical clubs and kickoff.
fn find_market_forecast<'a>(
    forecasts: &'a [MarketForecast],
    fixture: &Fixture,
    home: &'static str,
    away: &'static str,
) -> Option<&'a MarketForecast> {
    forecasts
        .iter()
        .filter(|forecast| {
            let difference = (forecast.commence_time - fixture.kickoff)
                .num_seconds()
                .abs();
            teams::canonical_name(&forecast.home_team) == Some(home)
                && teams::canonical_name(&forecast.away_team) == Some(away)
                && difference <= MARKET_KICKOFF_TOLERANCE_SECS
        })
        .min_by_key(|forecast| {
            (forecast.commence_time - fixture.kickoff)
                .num_seconds()
                .abs()
        })
}

/// Builds one season-ledger row from a history match and the local forecast.
///
/// When MPP reports a different pick from the local record, that pick is
/// authoritative and the local forecast metadata is discarded: its quotation
/// and source no longer describe what was scored.
fn season_match(
    clubs: &HashMap<String, Club>,
    historical: &HistoricalMatch,
    local: Option<&ForecastRecord>,
    now: DateTime<Utc>,
) -> Option<SeasonMatch> {
    let history_pick = historical.forecast.as_ref().map(|forecast| forecast.pick);
    let local = local.filter(|forecast| history_pick.is_none_or(|pick| pick == forecast.pick));
    let pick = history_pick.or_else(|| local.map(|forecast| forecast.pick))?;

    let result = historical
        .is_finished(now)
        .then(|| historical.score())
        .flatten();
    let odds = local
        .map(|forecast| forecast.odds)
        .or(historical.quotations);
    let computed_points = result
        .zip(odds)
        .map(|(result, odds)| model::points(pick, result, odds));
    let modal_points = match (result, odds, local.map(|forecast| forecast.modal)) {
        (Some(result), Some(odds), Some(modal)) => Some(model::points(modal, result, odds)),
        _ => None,
    };
    let points = historical
        .forecast
        .as_ref()
        .and_then(|forecast| forecast.points.as_ref())
        .map(points_record);

    Some(SeasonMatch {
        match_id: historical.id.clone(),
        kickoff: historical.kickoff,
        season: SeasonMatch::season_from_kickoff(historical.kickoff),
        home: club_name(clubs, &historical.home_club_id),
        away: club_name(clubs, &historical.away_club_id),
        pick: Some(pick),
        result,
        points,
        computed_points,
        modal_points,
        source: local.map(|forecast| forecast.source),
        odds,
        modal: local.map(|forecast| forecast.modal),
        status: historical.match_status.clone(),
        observed_at: now,
    })
}

fn club_name(clubs: &HashMap<String, Club>, club_id: &str) -> String {
    clubs
        .get(club_id)
        .map(|club| club.name.clone())
        .unwrap_or_else(|| club_id.to_owned())
}

fn points_record(points: &HistoryPoints) -> PointsRecord {
    PointsRecord {
        base: points.base,
        exact: points.exact,
        extra: points.extra,
        bonus: points.bonus,
        total: points.total,
        rarity_level: points.rarity_level.clone(),
    }
}

/// Compares the rule used by the model with MPP's own points breakdown.
///
/// `extra` and `bonus` are MPP additions the model does not price, so the
/// comparison uses `base + exact` when both are available and falls back to
/// the total otherwise.
fn verify_points(row: &SeasonMatch) {
    let (Some(points), Some(computed)) = (&row.points, row.computed_points) else {
        return;
    };
    let expected = points
        .base
        .zip(points.exact)
        .map_or(points.total, |(base, exact)| base + exact);

    if (computed - expected as f64).abs() > 0.5 {
        warn!(
            match_id = %row.match_id,
            app_total = points.total,
            computed,
            base = ?points.base,
            exact = ?points.exact,
            extra = ?points.extra,
            bonus = ?points.bonus,
            "computed points differ from MPP scoring"
        );
    }
}

/// Origin of the Elo ratings used by a trigger pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RatingsSource {
    /// Fetched during this pass.
    Fresh,
    /// Reused from the last successful fetch.
    Cached {
        /// Time of the fetch that produced the snapshot.
        fetched_at: DateTime<Utc>,
        /// Snapshot age at the start of the pass.
        age_seconds: i64,
    },
}

impl RatingsSource {
    fn is_cached(self) -> bool {
        matches!(self, Self::Cached { .. })
    }

    fn age_seconds(self) -> i64 {
        match self {
            Self::Fresh => 0,
            Self::Cached { age_seconds, .. } => age_seconds,
        }
    }

    fn observed_at(self, now: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            Self::Fresh => now,
            Self::Cached { fetched_at, .. } => fetched_at,
        }
    }
}

/// Source of the prediction used for one trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForecastSource {
    /// Market odds fetched during this pass.
    Market {
        /// Odds observation time reported by the selected bookmaker.
        observed_at: DateTime<Utc>,
    },
    /// Elo ratings, fresh or reused from the persisted cache.
    Elo {
        /// Observation time of the ratings.
        observed_at: DateTime<Utc>,
        /// Fresh or cached ratings.
        ratings: RatingsSource,
    },
}

impl ForecastSource {
    fn label(self) -> &'static str {
        match self {
            Self::Market { .. } => "market",
            Self::Elo {
                ratings: RatingsSource::Fresh,
                ..
            } => "elo",
            Self::Elo {
                ratings: RatingsSource::Cached { .. },
                ..
            } => "elo_cached",
        }
    }

    fn origin(self) -> ForecastOrigin {
        match self {
            Self::Market { .. } => ForecastOrigin::Market,
            Self::Elo {
                ratings: RatingsSource::Fresh,
                ..
            } => ForecastOrigin::Elo,
            Self::Elo {
                ratings: RatingsSource::Cached { .. },
                ..
            } => ForecastOrigin::EloCached,
        }
    }

    fn observed_at(self) -> DateTime<Utc> {
        match self {
            Self::Market { observed_at } | Self::Elo { observed_at, .. } => observed_at,
        }
    }

    fn elo_age_seconds(self) -> i64 {
        match self {
            Self::Market { .. } => 0,
            Self::Elo { ratings, .. } => ratings.age_seconds(),
        }
    }
}

fn forecast_record(
    prediction: &Prediction,
    fixture: &Fixture,
    source: ForecastSource,
) -> ForecastRecord {
    ForecastRecord {
        pick: prediction.pick,
        odds: fixture.odds,
        source: source.origin(),
        observed_at: source.observed_at(),
        lambdas: prediction.lambdas,
        modal: prediction.modal,
        modal_probability: prediction.modal_probability,
    }
}

fn log_trigger(
    fixture: &Fixture,
    trigger: Trigger,
    elo: Option<MatchElo>,
    prediction: &Prediction,
    remote: RemoteForecast,
    source: ForecastSource,
    status: &'static str,
) {
    info!(
        match_id = %fixture.id,
        home = %fixture.home.name,
        away = %fixture.away.name,
        kickoff = %fixture.kickoff,
        trigger = ?trigger,
        home_elo = elo.map(|elo| elo.home),
        away_elo = elo.map(|elo| elo.away),
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
        modal = %prediction.modal,
        modal_probability = prediction.modal_probability,
        recommendation = ?prediction.recommendation,
        recommendation_ev = prediction.recommendation_ev,
        remote = %remote,
        source = source.label(),
        observed_at = %source.observed_at(),
        elo_age_seconds = source.elo_age_seconds(),
        status,
        "MPP trigger"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_forecast_renders_each_read_outcome() {
        assert_eq!(RemoteForecast::NotRead.to_string(), "not_read");
        assert_eq!(RemoteForecast::Unreadable.to_string(), "unreadable");
        assert_eq!(RemoteForecast::Absent.to_string(), "absent");
        assert_eq!(
            RemoteForecast::Registered(Score { home: 1, away: 0 }).to_string(),
            "1-0"
        );
    }

    #[test]
    fn remote_forecast_only_exposes_a_registered_score() {
        assert_eq!(
            RemoteForecast::Registered(Score { home: 0, away: 1 }).score(),
            Some(Score { home: 0, away: 1 })
        );
        assert_eq!(RemoteForecast::NotRead.score(), None);
        assert_eq!(RemoteForecast::Unreadable.score(), None);
        assert_eq!(RemoteForecast::Absent.score(), None);
    }

    #[test]
    fn ratings_source_reports_freshness() {
        let fetched_at = "2026-10-19T04:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("valid timestamp");
        let cached = RatingsSource::Cached {
            fetched_at,
            age_seconds: 3_600,
        };

        assert_eq!(RatingsSource::Fresh.age_seconds(), 0);
        assert_eq!(cached.age_seconds(), 3_600);
        assert_eq!(cached.observed_at(fetched_at), fetched_at);
    }

    #[test]
    fn forecast_source_labels_market_and_cached_elo() {
        let observed_at = "2026-10-19T04:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("valid timestamp");
        let market = ForecastSource::Market { observed_at };
        let fresh = ForecastSource::Elo {
            observed_at,
            ratings: RatingsSource::Fresh,
        };
        let cached = ForecastSource::Elo {
            observed_at,
            ratings: RatingsSource::Cached {
                fetched_at: observed_at,
                age_seconds: 3_600,
            },
        };

        assert_eq!(market.label(), "market");
        assert_eq!(fresh.label(), "elo");
        assert_eq!(cached.label(), "elo_cached");
        assert_eq!(cached.observed_at(), observed_at);
    }
}
