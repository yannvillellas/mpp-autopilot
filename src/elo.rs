//! Elo ratings computed from Football-Data Ligue 1 results.
//!
//! Ratings are built from the `F1.csv` archive published by
//! <https://football-data.co.uk>, the backtest source; live odds come from MPP.
//! The archive is backfilled once from 2020/21, then each pass fetches the
//! current season and applies only results that are not already in the
//! persisted progress. Applied results are identified by season, real date
//! and clubs, so a restart or a re-run of a matchday never counts a match
//! twice. A later score correction from Football-Data is not re-applied:
//! replaying it could double count the match, which is worse than a missed
//! correction.
//!
//! The update rule is ClubElo's published method:
//!
//! ```text
//! expected = 1 / (1 + 10^(-((R_home + HFA - R_away) / SCALE)))
//! K        = BASE_K x margin(d)
//! R_home  += K x (S - expected)
//! R_away  -= K x (S - expected)
//! ```
//!
//! Football-Data is a third-party file: the first header carries a BOM, the
//! columns moved between seasons, dates are `dd/mm/yyyy`, and postponed
//! fixtures have empty scores. Columns are located by name and rows without a
//! complete date and score are skipped rather than treated as 0-0.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{Datelike, NaiveDate, Utc};
use reqwest::{Response, StatusCode};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use tracing::{info, warn};

use crate::teams;

/// Football-Data archive root; season codes are appended as `MMNN/F1.csv`.
const FOOTBALL_DATA_URL: &str = "https://football-data.co.uk/mmz4281";

/// First season of the one-time backfill, as a start year (2020/21).
pub const BACKFILL_FROM_SEASON: u16 = 2020;

/// Month in which a Ligue 1 season changes over.
const SEASON_START_MONTH: u32 = 7;

/// Rating scale: 400 points of difference represent 10:1 odds.
///
/// Fixed by the Elo system itself, not tuned on our data.
pub const SCALE: f64 = 400.0;

/// Rating of a club with no history.
///
/// Elo convention, not tuned on our data.
pub const START: f64 = 1500.0;

/// Home advantage, in Elo points.
///
/// ClubElo's published value; the football literature gives 60-80. Measured on
/// Ligue 1: every value from 40 to 90 gives a 0.9908-0.9910 log-loss, because
/// the calibration layer absorbs the shift. It sets the rating scale, not the
/// accuracy.
pub const HFA: f64 = 65.0;

/// Base K factor, controlling how fast a rating adapts.
///
/// ClubElo's published value. Measured on Ligue 1: K=10 gives 0.9902 against
/// 0.9909 for K=20, a paired difference of 0.0007 with a standard error of
/// 0.0028 (noise), while K=30 and K=40 degrade badly. Not tuned on our data.
pub const BASE_K: f64 = 20.0;

/// Goal-margin multiplier for a one-goal win (and a draw).
///
/// ClubElo's published value.
const ONE_GOAL_MARGIN: f64 = 1.0;

/// Goal-margin multiplier for a two-goal win.
///
/// ClubElo's published value.
const TWO_GOAL_MARGIN: f64 = 1.5;

/// Offset of the goal-margin multiplier used from three goals up: `(11 + d) / 8`.
///
/// ClubElo's published value.
const MARGIN_GOALS_OFFSET: f64 = 11.0;

/// Divisor of the goal-margin multiplier used from three goals up.
///
/// ClubElo's published value.
const MARGIN_GOALS_DIVISOR: f64 = 8.0;

/// Base of the logistic scale that turns a rating difference into odds.
const ODDS_BASE: f64 = 10.0;

/// Neutral term of the Elo expectation (zero adjusted difference gives 0.5).
const NEUTRAL_ODDS: f64 = 1.0;

/// Match score for a home win.
const HOME_WIN_SCORE: f64 = 1.0;

/// Match score for a draw.
const DRAW_SCORE: f64 = 0.5;

/// Match score for an away win.
const AWAY_WIN_SCORE: f64 = 0.0;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_BODY_CHARS: usize = 512;

/// One Football-Data result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    /// Start year of the season, for example 2020 for 2020/21.
    pub season: u16,
    /// Real match date.
    pub date: NaiveDate,
    /// Home club, in any spelling known to [`teams::canonical_name`].
    pub home: String,
    /// Away club, in any spelling known to [`teams::canonical_name`].
    pub away: String,
    /// Home goals.
    pub home_goals: u8,
    /// Away goals.
    pub away_goals: u8,
}

impl MatchResult {
    /// Orders results the way the backfill applies them.
    ///
    /// Postponed fixtures are not in chronological order in the CSV, so the
    /// real date comes first; season and clubs make the order deterministic
    /// when two matches share a date.
    pub fn chronological_cmp(&self, other: &Self) -> Ordering {
        self.date
            .cmp(&other.date)
            .then_with(|| self.season.cmp(&other.season))
            .then_with(|| self.home.cmp(&other.home))
            .then_with(|| self.away.cmp(&other.away))
    }
}

/// Decimal odds for one match, in the form Football-Data publishes.
///
/// The five prices are the 1X2 prices and the over/under prices of the 2.5
/// goals line. They are kept together because the market model needs both to
/// derive expected goals.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchOdds {
    /// Home-win price.
    pub home: f64,
    /// Draw price.
    pub draw: f64,
    /// Away-win price.
    pub away: f64,
    /// Over 2.5 goals price.
    pub over: f64,
    /// Under 2.5 goals price.
    pub under: f64,
}

/// The odds sets the backtest compares, parsed from one Football-Data row.
///
/// A set is [`None`] when any of its columns is absent or empty. This is how a
/// source that disappears from the archive is represented: the configuration
/// simply has no data, instead of the parse failing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SeasonOdds {
    /// Pinnacle closing 1X2 and 2.5-goal prices.
    pub pinnacle_closing: Option<MatchOdds>,
    /// Average closing 1X2 and 2.5-goal prices.
    pub average_closing: Option<MatchOdds>,
    /// Best closing 1X2 and 2.5-goal prices.
    pub max_closing: Option<MatchOdds>,
    /// Pinnacle pre-closing 1X2 prices with the closing 2.5-goal prices.
    ///
    /// Both Pinnacle sets share the closing totals so a pre-closing comparison
    /// isolates the observation time of the 1X2 prices.
    pub pinnacle_pre_closing: Option<MatchOdds>,
}

/// One completed Football-Data match with the odds sets found in its row.
#[derive(Debug, Clone, PartialEq)]
pub struct SeasonMatch {
    /// Score, date and clubs.
    pub result: MatchResult,
    /// Odds sets present in the row.
    pub odds: SeasonOdds,
}

/// Elo ratings indexed by normalized club name.
///
/// The type also carries the progress needed to update the ratings
/// incrementally: the matches already applied and the last season downloaded.
/// [`crate::state::State::record_elo`] persists that progress separately from
/// the ratings map, so a restart resumes instead of replaying the backfill.
#[derive(Debug, Clone, Default)]
pub struct EloRatings {
    pub(crate) ratings: BTreeMap<String, f64>,
    pub(crate) applied: BTreeSet<String>,
    pub(crate) backfilled_through: Option<u16>,
}

/// Compares ratings only; incremental progress is persisted separately by
/// [`crate::state::State`].
impl PartialEq for EloRatings {
    fn eq(&self, other: &Self) -> bool {
        self.ratings == other.ratings
    }
}

impl Serialize for EloRatings {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.ratings.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EloRatings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let ratings = BTreeMap::<String, f64>::deserialize(deserializer)?;
        Ok(Self::from_pairs(ratings))
    }
}

impl EloRatings {
    /// Builds ratings from `(club name, Elo)` pairs.
    ///
    /// Known provider names are resolved to their canonical Ligue 1 name and
    /// keys are normalized with [`teams::normalize_name`]. Non-finite or
    /// non-positive values are ignored. Later pairs replace earlier ones for
    /// the same normalized name.
    pub fn from_pairs<I, S>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (S, f64)>,
        S: AsRef<str>,
    {
        let mut ratings = BTreeMap::new();

        for (name, elo) in pairs {
            if !elo.is_finite() || elo <= 0.0 {
                continue;
            }
            ratings.insert(club_key(name.as_ref()), elo);
        }

        Self {
            ratings,
            ..Self::default()
        }
    }

    /// Returns the rating for a canonical Ligue 1 club.
    ///
    /// Provider spellings are resolved through [`teams::canonical_name`].
    pub fn rating(&self, canonical: &str) -> Option<f64> {
        self.ratings.get(&club_key(canonical)).copied()
    }

    /// Number of clubs in the index.
    pub fn len(&self) -> usize {
        self.ratings.len()
    }

    /// Returns whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.ratings.is_empty()
    }

    /// Applies results in chronological order and returns how many were new.
    ///
    /// Results are sorted by real date before the ratings move, because
    /// postponed fixtures are not in order in the CSV. A result already in the
    /// applied set is ignored, so applying the same match twice changes
    /// nothing.
    pub fn apply_results<I>(&mut self, results: I) -> usize
    where
        I: IntoIterator<Item = MatchResult>,
    {
        let mut results: Vec<_> = results.into_iter().collect();
        results.sort_by(MatchResult::chronological_cmp);

        results
            .into_iter()
            .filter(|result| self.apply_result(result))
            .count()
    }

    /// Applies one result with an explicit home advantage and K factor.
    ///
    /// The rule is the same as [`Self::apply_results`], which uses [`HFA`] and
    /// [`BASE_K`]. The backtest uses this entry point to replay the history
    /// with alternative values without copying the update rule. A non-finite
    /// home advantage or a negative or non-finite K leaves the ratings
    /// unchanged.
    pub fn apply_with(&mut self, result: &MatchResult, hfa: f64, k: f64) -> bool {
        if !hfa.is_finite() || !k.is_finite() || k < 0.0 {
            return false;
        }

        let home = club_key(&result.home);
        let away = club_key(&result.away);
        let key = match_key(result.season, result.date, &home, &away);

        if !self.applied.insert(key) {
            return false;
        }

        let home_rating = self.ratings.get(&home).copied().unwrap_or(START);
        let away_rating = self.ratings.get(&away).copied().unwrap_or(START);
        let expected = expected_home_score_with(home_rating, away_rating, hfa);
        let margin = margin(result.home_goals.abs_diff(result.away_goals));
        let score = result_score(result.home_goals, result.away_goals);
        let delta = k * margin * (score - expected);

        self.ratings.insert(home, home_rating + delta);
        self.ratings.insert(away, away_rating - delta);
        true
    }

    fn apply_result(&mut self, result: &MatchResult) -> bool {
        self.apply_with(result, HFA, BASE_K)
    }
}

/// Elo ratings source used by the orchestrator and tests.
#[allow(
    async_fn_in_trait,
    reason = "the trait is used generically with Send + Sync clients"
)]
pub trait EloSite: Send + Sync {
    /// Fetches the current club ratings.
    async fn fetch_ratings(&self) -> Result<EloRatings, EloError>;
}

/// HTTP client for the Football-Data Ligue 1 archive.
pub struct FootballDataClient {
    http: reqwest::Client,
    base_url: String,
    cached: Mutex<EloRatings>,
}

impl FootballDataClient {
    /// Builds a client with a 30-second request timeout.
    ///
    /// `cached` is the persisted rating state. It is used only when its
    /// backfill completed, so ClubElo ratings cached by an older version are
    /// never mixed with ratings computed from Football-Data results.
    pub fn new(cached: EloRatings) -> Result<Self, EloError> {
        let cached = if cached.backfilled_through.is_some() && !cached.applied.is_empty() {
            cached
        } else {
            EloRatings::default()
        };
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(EloError::ClientBuild)?;
        Ok(Self {
            http,
            base_url: FOOTBALL_DATA_URL.to_owned(),
            cached: Mutex::new(cached),
        })
    }

    // Test-only seam. Integration tests compile the library without
    // `cfg(test)`, so the wiremock tests must stay in this module. Do not move
    // them to `tests/` without making this constructor a deliberate public
    // API.
    #[cfg(test)]
    fn with_base_url(cached: EloRatings, base_url: impl Into<String>) -> Result<Self, EloError> {
        let mut client = Self::new(cached)?;
        client.base_url = base_url.into();
        Ok(client)
    }

    fn snapshot(&self) -> EloRatings {
        self.cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn store(&self, ratings: EloRatings) {
        *self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ratings;
    }

    fn season_url(&self, season: u16) -> String {
        format!("{}{}", self.base_url, season_path(season))
    }

    /// Downloads one season CSV from the Football-Data archive.
    ///
    /// The raw body is returned unchanged so a caller that caches the archive,
    /// such as the backtest, can fingerprint it and parse it with
    /// [`parse_season_matches`]. The production ratings path uses the same
    /// request through [`Self::fetch_season`].
    pub async fn fetch_season_csv(&self, season: u16) -> Result<String, EloError> {
        let response = self
            .http
            .get(self.season_url(season))
            .header("accept", "text/csv")
            .send()
            .await
            .map_err(EloError::Network)?;

        let response = check_response(response).await?;
        response.text().await.map_err(EloError::Network)
    }

    async fn fetch_season(&self, season: u16) -> Result<Vec<MatchResult>, EloError> {
        let body = self.fetch_season_csv(season).await?;
        parse_season(season, &body)
    }
}

impl EloSite for FootballDataClient {
    async fn fetch_ratings(&self) -> Result<EloRatings, EloError> {
        let base = self.snapshot();
        let target = current_season(Utc::now().date_naive());
        let first = first_season_to_fetch(base.backfilled_through, target);
        let mut next = base;
        let is_backfill = next.backfilled_through.is_none();
        let mut applied = 0;

        for season in first..=target {
            match self.fetch_season(season).await {
                Ok(results) => applied += next.apply_results(results),
                Err(error)
                    if season == target
                        && is_backfill
                        && matches!(
                            error,
                            EloError::Status {
                                status: StatusCode::NOT_FOUND,
                                ..
                            }
                        ) =>
                {
                    warn!(
                        season,
                        "Football-Data season file is not published yet; keeping the previous ratings"
                    );
                }
                Err(error) => return Err(error),
            }
        }

        if next.is_empty() {
            return Err(EloError::NoRatings);
        }

        if is_backfill {
            info!(
                season = target,
                applied, "backfilled Elo ratings from Football-Data"
            );
        }

        next.backfilled_through = Some(
            next.backfilled_through
                .map_or(target, |season| season.max(target)),
        );
        let ratings = next.clone();
        self.store(next);
        Ok(ratings)
    }
}

/// Errors returned by the Football-Data client.
#[derive(Debug, Error)]
pub enum EloError {
    /// The HTTP request failed.
    #[error("Football-Data request failed: {0}")]
    Network(#[source] reqwest::Error),
    /// The HTTP client could not be built.
    #[error("failed to build the Football-Data HTTP client: {0}")]
    ClientBuild(#[source] reqwest::Error),
    /// The archive returned a non-success status.
    #[error("Football-Data returned HTTP {status}: {body}")]
    Status {
        /// HTTP status.
        status: StatusCode,
        /// Truncated response body.
        body: String,
    },
    /// A season CSV is missing required columns.
    #[error("Football-Data season {season} CSV is invalid: {reason}")]
    InvalidCsv {
        /// Season start year.
        season: u16,
        /// Short description of the invalid structure.
        reason: &'static str,
    },
    /// No result produced any rating.
    #[error("no Football-Data ratings found")]
    NoRatings,
}

impl EloError {
    /// Returns whether the operation may succeed later.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Network(_) => true,
            Self::Status { status, .. } => {
                status.is_server_error()
                    || matches!(
                        *status,
                        StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT
                    )
            }
            Self::ClientBuild(_) | Self::InvalidCsv { .. } | Self::NoRatings => false,
        }
    }
}

fn expected_home_score_with(home: f64, away: f64, hfa: f64) -> f64 {
    let difference = home + hfa - away;
    NEUTRAL_ODDS / (NEUTRAL_ODDS + ODDS_BASE.powf(-difference / SCALE))
}

fn margin(goal_difference: u8) -> f64 {
    match goal_difference {
        0 | 1 => ONE_GOAL_MARGIN,
        2 => TWO_GOAL_MARGIN,
        goals => (MARGIN_GOALS_OFFSET + f64::from(goals)) / MARGIN_GOALS_DIVISOR,
    }
}

fn result_score(home_goals: u8, away_goals: u8) -> f64 {
    match home_goals.cmp(&away_goals) {
        Ordering::Greater => HOME_WIN_SCORE,
        Ordering::Equal => DRAW_SCORE,
        Ordering::Less => AWAY_WIN_SCORE,
    }
}

fn club_key(name: &str) -> String {
    let canonical = teams::canonical_name(name).unwrap_or(name);
    teams::normalize_name(canonical)
}

fn match_key(season: u16, date: NaiveDate, home: &str, away: &str) -> String {
    format!("{season}|{date}|{home}|{away}")
}

fn current_season(date: NaiveDate) -> u16 {
    let year = date.year();
    if date.month() >= SEASON_START_MONTH {
        year as u16
    } else {
        (year - 1) as u16
    }
}

fn first_season_to_fetch(backfilled_through: Option<u16>, target: u16) -> u16 {
    backfilled_through.map_or(BACKFILL_FROM_SEASON, |season| season.min(target))
}

fn season_path(season: u16) -> String {
    format!("/{:02}{:02}/F1.csv", season % 100, (season + 1) % 100)
}

fn parse_season(season: u16, csv: &str) -> Result<Vec<MatchResult>, EloError> {
    Ok(parse_season_matches(season, csv)?
        .into_iter()
        .map(|matched| matched.result)
        .collect())
}

/// Parses one Football-Data season with the odds sets used by the backtest.
///
/// This is the production parser: a BOM is stripped, the header may carry
/// carriage returns, columns are located by name, and rows without a complete
/// date and score are skipped. Odds columns that are absent from a season or
/// empty in a row leave the corresponding [`SeasonOdds`] field at [`None`].
pub fn parse_season_matches(season: u16, csv: &str) -> Result<Vec<SeasonMatch>, EloError> {
    let csv = csv.strip_prefix('\u{feff}').unwrap_or(csv);
    let mut lines = csv.lines();
    let header = lines
        .next()
        .ok_or(EloError::InvalidCsv {
            season,
            reason: "empty file",
        })
        .map(parse_csv_line)?;
    let columns = Columns::from_header(season, &header)?;

    let mut matches = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(matched) = columns.parse_row(season, &parse_csv_line(line)) {
            matches.push(matched);
        }
    }
    Ok(matches)
}

struct Columns {
    date: usize,
    home: usize,
    away: usize,
    home_goals: usize,
    away_goals: usize,
    pinnacle_closing: Option<OddsColumns>,
    average_closing: Option<OddsColumns>,
    max_closing: Option<OddsColumns>,
    pinnacle_pre_closing: Option<OddsColumns>,
}

#[derive(Clone, Copy)]
struct OddsColumns {
    home: usize,
    draw: usize,
    away: usize,
    over: usize,
    under: usize,
}

impl Columns {
    fn from_header(season: u16, header: &[String]) -> Result<Self, EloError> {
        let find = |name: &str| {
            header
                .iter()
                .position(|column| column.trim().eq_ignore_ascii_case(name))
        };
        let odds = |home: &str, draw: &str, away: &str, over: &str, under: &str| {
            Some(OddsColumns {
                home: find(home)?,
                draw: find(draw)?,
                away: find(away)?,
                over: find(over)?,
                under: find(under)?,
            })
        };

        let missing = |name: &str| EloError::InvalidCsv {
            season,
            reason: match name {
                "Date" => "missing Date column",
                "HomeTeam" => "missing HomeTeam column",
                "AwayTeam" => "missing AwayTeam column",
                "FTHG" => "missing FTHG column",
                "FTAG" => "missing FTAG column",
                _ => "missing required column",
            },
        };

        Ok(Self {
            date: find("Date").ok_or_else(|| missing("Date"))?,
            home: find("HomeTeam").ok_or_else(|| missing("HomeTeam"))?,
            away: find("AwayTeam").ok_or_else(|| missing("AwayTeam"))?,
            home_goals: find("FTHG").ok_or_else(|| missing("FTHG"))?,
            away_goals: find("FTAG").ok_or_else(|| missing("FTAG"))?,
            pinnacle_closing: odds("PSCH", "PSCD", "PSCA", "PC>2.5", "PC<2.5"),
            average_closing: odds("AvgCH", "AvgCD", "AvgCA", "AvgC>2.5", "AvgC<2.5"),
            max_closing: odds("MaxCH", "MaxCD", "MaxCA", "MaxC>2.5", "MaxC<2.5"),
            pinnacle_pre_closing: odds("PSH", "PSD", "PSA", "PC>2.5", "PC<2.5"),
        })
    }

    fn parse_row(&self, season: u16, fields: &[String]) -> Option<SeasonMatch> {
        let date = parse_date(fields.get(self.date)?)?;
        let home = fields.get(self.home)?.trim();
        let away = fields.get(self.away)?.trim();
        if home.is_empty() || away.is_empty() {
            return None;
        }

        Some(SeasonMatch {
            result: MatchResult {
                season,
                date,
                home: home.to_owned(),
                away: away.to_owned(),
                home_goals: fields.get(self.home_goals)?.trim().parse().ok()?,
                away_goals: fields.get(self.away_goals)?.trim().parse().ok()?,
            },
            odds: SeasonOdds {
                pinnacle_closing: self
                    .pinnacle_closing
                    .and_then(|columns| columns.parse(fields)),
                average_closing: self
                    .average_closing
                    .and_then(|columns| columns.parse(fields)),
                max_closing: self.max_closing.and_then(|columns| columns.parse(fields)),
                pinnacle_pre_closing: self
                    .pinnacle_pre_closing
                    .and_then(|columns| columns.parse(fields)),
            },
        })
    }
}

impl OddsColumns {
    fn parse(self, fields: &[String]) -> Option<MatchOdds> {
        let price = |index: usize| {
            fields
                .get(index)?
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|price| price.is_finite() && *price > 1.0)
        };

        Some(MatchOdds {
            home: price(self.home)?,
            draw: price(self.draw)?,
            away: price(self.away)?,
            over: price(self.over)?,
            under: price(self.under)?,
        })
    }
}

fn parse_date(value: &str) -> Option<NaiveDate> {
    let value = value.trim();
    match value.rsplit('/').next()?.len() {
        4 => NaiveDate::parse_from_str(value, "%d/%m/%Y").ok(),
        2 => NaiveDate::parse_from_str(value, "%d/%m/%y").ok(),
        _ => None,
    }
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut characters = line.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '"' if quoted => {
                if characters.peek() == Some(&'"') {
                    field.push('"');
                    characters.next();
                } else {
                    quoted = false;
                }
            }
            '"' if field.is_empty() => quoted = true,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            _ => field.push(character),
        }
    }
    fields.push(field);
    fields
}

async fn check_response(response: Response) -> Result<Response, EloError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(EloError::Status {
            status,
            body: truncate_body(&body),
        });
    }
    Ok(response)
}

fn truncate_body(body: &str) -> String {
    let mut chars = body.chars();
    let mut truncated: String = chars.by_ref().take(MAX_ERROR_BODY_CHARS).collect();
    if chars.next().is_some() {
        truncated.push('…');
    }
    truncated
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::state::{Mode, State};

    const CSV: &str = "\u{feff}Div,Date,Time,HomeTeam,AwayTeam,FTHG,FTAG,FTR\n\
        F1,21/08/2020,18:00,Bordeaux,Nantes,0,0,D\n\
        F1,22/08/2020,18:00,Paris SG,Marseille,,,\n\
        F1,23/08/2020,18:00,Lens,Lille,2,1,H\n\
        F1,32/13/2020,18:00,Brest,Lyon,1,0,H\n";

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
    }

    fn temp_state_path() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "mpp-autopilot-elo-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("state.json");
        (dir, path)
    }

    fn season_csv(season: u16) -> String {
        format!(
            "Div,Date,HomeTeam,AwayTeam,FTHG,FTAG,FTR\n\
             F1,01/09/{season},Paris SG,Marseille,1,0,H\n"
        )
    }

    async fn mount_season(server: &MockServer, season: u16) {
        Mock::given(method("GET"))
            .and(path(season_path(season)))
            .respond_with(ResponseTemplate::new(200).set_body_string(season_csv(season)))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn a_restored_backfill_only_fetches_the_current_season() {
        let server = MockServer::start().await;
        let target = current_season(Utc::now().date_naive());
        let seasons: Vec<u16> = (BACKFILL_FROM_SEASON..=target).collect();
        let paths: Vec<String> = seasons.iter().copied().map(season_path).collect();

        for season in seasons {
            mount_season(&server, season).await;
        }

        let first_client =
            FootballDataClient::with_base_url(EloRatings::default(), server.uri()).expect("client");
        let first = first_client.fetch_ratings().await.expect("backfill");
        assert!(first.rating("PSG").is_some());

        let requests = server.received_requests().await.expect("recorded requests");
        let first_paths: Vec<_> = requests.iter().map(|request| request.url.path()).collect();
        assert_eq!(
            first_paths,
            paths.iter().map(String::as_str).collect::<Vec<_>>()
        );

        let (dir, state_file) = temp_state_path();
        let mut state = State::new(Mode::Dry);
        state.record_elo(first.clone(), Utc::now());
        state.save(&state_file).expect("save state");
        let loaded = State::load(&state_file, Mode::Dry).expect("load state");
        let _ = std::fs::remove_dir_all(dir);

        let second_client =
            FootballDataClient::with_base_url(loaded.cached_elo(), server.uri()).expect("client");
        let second = second_client.fetch_ratings().await.expect("incremental");
        assert_eq!(second, first);

        let requests = server.received_requests().await.expect("recorded requests");
        let later_paths: Vec<_> = requests[paths.len()..]
            .iter()
            .map(|request| request.url.path().to_owned())
            .collect();
        assert_eq!(later_paths, vec![season_path(target)]);
    }

    #[tokio::test]
    async fn a_missing_current_season_fails_once_the_backfill_exists() {
        let target = current_season(Utc::now().date_naive());

        let backfill_server = MockServer::start().await;
        for season in BACKFILL_FROM_SEASON..=target {
            mount_season(&backfill_server, season).await;
        }
        let backfill =
            FootballDataClient::with_base_url(EloRatings::default(), backfill_server.uri())
                .expect("client")
                .fetch_ratings()
                .await
                .expect("backfill");
        assert!(backfill.rating("PSG").is_some());

        let missing_server = MockServer::start().await;
        for season in BACKFILL_FROM_SEASON..target {
            mount_season(&missing_server, season).await;
        }
        let client =
            FootballDataClient::with_base_url(backfill, missing_server.uri()).expect("client");
        let error = client
            .fetch_ratings()
            .await
            .expect_err("missing current season");

        assert!(matches!(
            error,
            EloError::Status {
                status: StatusCode::NOT_FOUND,
                ..
            }
        ));
        let requests = missing_server
            .received_requests()
            .await
            .expect("recorded requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path().to_owned(), season_path(target));
    }

    #[test]
    fn margin_follows_the_clubelo_convention() {
        assert_eq!(margin(0), ONE_GOAL_MARGIN);
        assert_eq!(margin(1), ONE_GOAL_MARGIN);
        assert_eq!(margin(2), TWO_GOAL_MARGIN);
        assert_eq!(margin(3), 1.75);
        assert_eq!(margin(4), 1.875);
    }

    #[test]
    fn expected_score_includes_home_advantage() {
        let expected = expected_home_score_with(START, START, HFA);
        let without_advantage = 1.0 / (1.0 + 10f64.powf(-HFA / SCALE));

        assert!((expected - without_advantage).abs() < 1e-12);
        assert!(expected > 0.5);
    }

    #[test]
    fn parser_skips_bom_empty_scores_and_invalid_dates() {
        let results = parse_season(2020, CSV).expect("parse CSV");

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].home, "Bordeaux");
        assert_eq!(results[0].date, date(2020, 8, 21));
        assert_eq!(results[1].home, "Lens");
        assert_eq!(results[1].home_goals, 2);
    }

    #[test]
    fn parser_locates_columns_by_name_across_seasons() {
        let csv = "Date,Div,HomeTeam,FTHG,AwayTeam,FTAG\n\
            15/08/25,F1,Le Havre,1,Auxerre,0\n";

        let results = parse_season(2025, csv).expect("parse CSV");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].date, date(2025, 8, 15));
        assert_eq!(results[0].home, "Le Havre");
        assert_eq!(results[0].away, "Auxerre");
        assert_eq!(results[0].away_goals, 0);
    }

    #[test]
    fn parser_reads_the_backtest_odds_sets_and_leaves_empty_prices_out() {
        let csv = "Div,Date,HomeTeam,AwayTeam,FTHG,FTAG,PSCH,PSCD,PSCA,PC>2.5,PC<2.5,\
                   PSH,PSD,PSA,P>2.5,P<2.5,AvgCH,AvgCD,AvgCA,AvgC>2.5,AvgC<2.5\r\n\
                   F1,16/08/2024,Le Havre,Paris SG,1,4,7.8,5.0,1.35,1.71,2.10,\
                   7.5,5.1,1.40,1.30,3.00,7.3,4.9,1.37,1.69,2.13\r\n\
                   F1,17/08/2024,Brest,Marseille,1,5,3.3,3.6,2.1,,,\
                   3.2,3.55,2.2,1.95,1.85,3.16,3.5,2.15,2.0,1.80\r\n";

        let matches = parse_season_matches(2024, csv).expect("parse CSV");

        assert_eq!(matches.len(), 2);
        let pinnacle = matches[0].odds.pinnacle_closing.expect("Pinnacle odds");
        assert!((pinnacle.home - 7.8).abs() < f64::EPSILON);
        assert!((pinnacle.over - 1.71).abs() < f64::EPSILON);
        let pre = matches[0]
            .odds
            .pinnacle_pre_closing
            .expect("Pinnacle pre-closing odds");
        assert!((pre.home - 7.5).abs() < f64::EPSILON);
        assert!((pre.over - 1.71).abs() < f64::EPSILON);
        assert!(matches[1].odds.pinnacle_closing.is_none());
        assert!(matches[1].odds.pinnacle_pre_closing.is_none());
        assert!(matches[1].odds.average_closing.is_some());
        assert!(matches[0].odds.max_closing.is_none());
    }

    #[test]
    fn parser_rejects_files_without_the_required_columns() {
        let error = parse_season(2020, "Div,HomeTeam,AwayTeam\nF1,Lens,Lille\n")
            .expect_err("missing columns");

        assert!(matches!(
            error,
            EloError::InvalidCsv {
                season: 2020,
                reason: "missing Date column"
            }
        ));
    }

    #[test]
    fn csv_fields_support_quotes_and_commas() {
        let fields = parse_csv_line(r#"F1,"Paris, SG",Le Havre,"1""#);

        assert_eq!(fields, ["F1", "Paris, SG", "Le Havre", "1"]);
    }

    #[test]
    fn season_paths_match_the_archive_layout() {
        assert_eq!(season_path(2020), "/2021/F1.csv");
        assert_eq!(season_path(2026), "/2627/F1.csv");
    }

    #[test]
    fn season_changes_in_july() {
        assert_eq!(current_season(date(2026, 6, 30)), 2025);
        assert_eq!(current_season(date(2026, 7, 1)), 2026);
    }

    #[test]
    fn only_the_missing_seasons_are_fetched_after_a_backfill() {
        assert_eq!(first_season_to_fetch(None, 2026), BACKFILL_FROM_SEASON);
        assert_eq!(first_season_to_fetch(Some(2026), 2026), 2026);
        assert_eq!(first_season_to_fetch(Some(2024), 2026), 2024);
        assert_eq!(first_season_to_fetch(Some(2027), 2026), 2026);
    }
}
