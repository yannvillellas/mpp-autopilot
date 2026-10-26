//! Backtest harness for the Ligue 1 prediction pipeline.
//!
//! The harness replays completed Football-Data seasons through the same code
//! the bot uses: [`crate::odds::expected_goals`] turns the market prices into
//! expected goals, [`crate::model::predict_from_lambdas`] builds the score grid
//! and [`crate::model::score_matrix`] exposes it for scoring, and
//! [`crate::elo::EloRatings`] replays the production backfill match by match.
//! Ratings are read before the match is applied, so a prediction never sees
//! the result it is measured on.
//!
//! The season parser is the production one ([`crate::elo::parse_season_matches`]);
//! it handles the BOM, the header quirks, missing columns and postponed
//! fixtures. The harness only adds the odds columns it needs.
//!
//! Metrics are the mean exact-score log-loss, the mean 1X2 log-loss and the
//! exact-score hit rate. Historical MPP quotations are not published, so the
//! configuration's own 1X2 prices stand in for them when the model picks an
//! exact score (the average closing price when the row is Elo); this affects
//! only the hit column. The production score grid
//! stops at [`crate::model::MAX_GOALS`], so a rarer score is evaluated at the
//! grid's last bucket to keep the log-loss finite; the hit column still
//! compares the real score.
//!
//! The reference check reads `backtest-reference.json`, which the first full
//! run writes. A configuration that is no longer present in the archive is
//! skipped rather than failing the check, so a bookmaker can disappear.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::elo::{
    BACKFILL_FROM_SEASON, EloError, EloRatings, FootballDataClient, MatchOdds, MatchResult,
    SeasonMatch, SeasonOdds, parse_season_matches,
};
use crate::model::{self, Odds, Outcome, Prediction, Score};
use crate::odds;

pub mod tune;

/// Directory where downloaded Football-Data seasons are cached.
pub const DATA_DIR: &str = "backtest-data";

/// Season used for the reference check.
pub const REFERENCE_PATH: &str = "backtest-reference.json";

/// First season of the evaluation window (2024/25).
pub const EVALUATION_FROM_SEASON: u16 = 2024;

/// Last season of the evaluation window (2025/26).
pub const EVALUATION_TO_SEASON: u16 = 2025;

/// Absolute tolerance applied to every metric by [`check`].
pub const TOLERANCE: f64 = 1e-3;

/// Working noise floor for a log-loss difference, in nats.
///
/// Issue #16 sets it at roughly 0.01 nat; the paired intervals are the
/// authoritative test for a given comparison.
pub const NOISE_FLOOR: f64 = 0.01;

const REFERENCE_VERSION: u32 = 2;
const CONFIDENCE_Z: f64 = 1.96;

/// Run options selected on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Directory where season files are cached.
    pub data_dir: PathBuf,
    /// Download every season again instead of using the cache.
    pub refresh: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(DATA_DIR),
            refresh: false,
        }
    }
}

/// A market odds source measured by the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Configuration {
    /// Pinnacle closing 1X2 and closing total.
    PinnacleClosing,
    /// Average closing 1X2 and closing total.
    AverageClosing,
    /// Best closing 1X2 and closing total.
    MaxClosing,
    /// Pinnacle pre-closing 1X2 and pre-closing total.
    PinnaclePreClosing,
    /// The bot's own Elo rating path.
    Elo,
}

impl Configuration {
    /// Every configuration reported by the harness.
    pub const ALL: [Self; 5] = [
        Self::PinnacleClosing,
        Self::AverageClosing,
        Self::MaxClosing,
        Self::PinnaclePreClosing,
        Self::Elo,
    ];

    /// Stable identifier used in the reference file.
    pub fn key(self) -> &'static str {
        match self {
            Self::PinnacleClosing => "pinnacle_closing",
            Self::AverageClosing => "average_closing",
            Self::MaxClosing => "max_closing",
            Self::PinnaclePreClosing => "pinnacle_pre_closing",
            Self::Elo => "elo",
        }
    }

    /// Human-readable label used in the report.
    pub fn label(self) -> &'static str {
        match self {
            Self::PinnacleClosing => "market / pinnacle closing",
            Self::AverageClosing => "market / average closing",
            Self::MaxClosing => "market / best price closing",
            Self::PinnaclePreClosing => "market / pinnacle pre-closing",
            Self::Elo => "Elo",
        }
    }

    fn odds(self, odds: &SeasonOdds) -> Option<MatchOdds> {
        match self {
            Self::PinnacleClosing => odds.pinnacle_closing,
            Self::AverageClosing => odds.average_closing,
            Self::MaxClosing => odds.max_closing,
            Self::PinnaclePreClosing => odds.pinnacle_pre_closing,
            Self::Elo => None,
        }
    }
}

/// Paired comparison of one configuration with the reference configuration.
///
/// For every match covered by both rows, `delta` is the reference exact-score
/// loss minus the row's exact-score loss. A negative delta means the row is
/// worse than the reference. The interval is `mean(delta) +/- 1.96 * se`,
/// with `se` the sample standard deviation of the paired differences divided
/// by the square root of their count.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PairedComparison {
    /// Mean paired difference (reference minus row).
    pub delta: f64,
    /// Lower bound of the 95% interval, when at least two pairs exist.
    pub ci_low: Option<f64>,
    /// Upper bound of the 95% interval, when at least two pairs exist.
    pub ci_high: Option<f64>,
    /// Number of paired matches.
    pub n: usize,
}

/// Mean metrics for one configuration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// Mean exact-score log-loss.
    pub exact: f64,
    /// Mean 1X2 log-loss.
    pub one_x_two: f64,
    /// Share of exact-score picks that matched the result.
    pub hit: f64,
    /// Number of matches included.
    pub n: usize,
    /// Paired comparison with the reference configuration.
    pub paired: Option<PairedComparison>,
}

/// One configuration and its metrics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConfigurationReport {
    /// Measured configuration.
    pub configuration: Configuration,
    /// Metrics over the evaluation window.
    pub metrics: Metrics,
}

/// One downloaded season file as printed in the report.
#[derive(Debug, Clone, PartialEq)]
pub struct DataFileReport {
    /// Start year of the season.
    pub season: u16,
    /// Cached path.
    pub path: PathBuf,
    /// Stable fingerprint of the raw file.
    pub fingerprint: String,
    /// Number of completed matches parsed.
    pub matches: usize,
    /// First and last match date in the file.
    pub range: Option<DateRange>,
}

/// Inclusive date range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateRange {
    /// First date.
    pub from: NaiveDate,
    /// Last date.
    pub to: NaiveDate,
}

impl fmt::Display for DateRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.from, self.to)
    }
}

/// Complete backtest report.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// One row per configuration that had data.
    pub configurations: Vec<ConfigurationReport>,
    /// One row per downloaded season.
    pub files: Vec<DataFileReport>,
    /// Date range of the evaluated matches.
    pub evaluation: Option<DateRange>,
    /// Number of matches in the evaluation window.
    pub evaluation_matches: usize,
}

impl Report {
    /// Prints the table, then the file fingerprints and date ranges.
    pub fn print(&self) {
        println!(
            "{:<30} {:>8} {:>8} {:>8} {:>6} {:>9} {:>22} {:>7}",
            "configuration", "exact", "1x2", "hit", "n", "delta ref", "95% CI", "paired"
        );
        for row in &self.configurations {
            let (delta, interval, paired) = format_comparison(row.metrics.paired);
            println!(
                "{:<30} {:>8.4} {:>8.4} {:>8.4} {:>6} {:>9} {:>22} {:>7}",
                row.configuration.label(),
                row.metrics.exact,
                row.metrics.one_x_two,
                row.metrics.hit,
                row.metrics.n,
                delta,
                interval,
                paired
            );
        }
        println!(
            "delta ref = market / pinnacle closing loss - row loss; negative means the row is worse"
        );
        println!(
            "noise floor: {NOISE_FLOOR:.2} nat; a difference below it, or a 95% CI containing 0, is not a result"
        );

        println!();
        println!("data files:");
        for file in &self.files {
            let range = file.range.map_or_else(
                || "no completed matches".to_owned(),
                |range| range.to_string(),
            );
            println!(
                "  {}/{}  {}  {}  {}  {} matches",
                file.season,
                file.season + 1,
                file.path.display(),
                file.fingerprint,
                range,
                file.matches
            );
        }
        if let Some(range) = self.evaluation {
            println!(
                "evaluation range: {range} ({} matches)",
                self.evaluation_matches
            );
        }
    }
}

/// Committed reference metrics, compared by [`check`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    /// Reference format version.
    pub version: u32,
    /// First evaluated season.
    pub from_season: u16,
    /// Last evaluated season.
    pub to_season: u16,
    /// Documented tolerance, in absolute metric points.
    pub tolerance: f64,
    /// Metrics per configuration, keyed by [`Configuration::key`].
    pub configurations: BTreeMap<String, Metrics>,
}

impl Report {
    /// Builds the reference that [`check`] compares against.
    pub fn reference(&self) -> Reference {
        Reference {
            version: REFERENCE_VERSION,
            from_season: EVALUATION_FROM_SEASON,
            to_season: EVALUATION_TO_SEASON,
            tolerance: TOLERANCE,
            configurations: self
                .configurations
                .iter()
                .map(|row| (row.configuration.key().to_owned(), row.metrics))
                .collect(),
        }
    }

    fn row(&self, key: &str) -> Option<&ConfigurationReport> {
        self.configurations
            .iter()
            .find(|row| row.configuration.key() == key)
    }
}

/// Result of a successful reference comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Number of configurations that were present and compared.
    pub checked: usize,
    /// Reference configurations absent from the report.
    pub skipped: Vec<String>,
}

/// Error returned when the report does not match the reference.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum CheckError {
    /// The reference was produced by an incompatible version.
    #[error("reference version {found} is not supported (expected {expected})")]
    Version {
        /// Version found in the file.
        found: u32,
        /// Version this binary understands.
        expected: u32,
    },
    /// The reference was produced on another evaluation window.
    #[error(
        "reference window is {from_season}/{to_season}, expected {expected_from}/{expected_to}"
    )]
    Window {
        /// First season in the reference.
        from_season: u16,
        /// Last season in the reference.
        to_season: u16,
        /// First season this binary evaluates.
        expected_from: u16,
        /// Last season this binary evaluates.
        expected_to: u16,
    },
    /// The report has a different number of matches for a configuration.
    #[error("configuration {key}: expected n={expected}, got n={actual}")]
    Count {
        /// Configuration key.
        key: String,
        /// Match count in the reference.
        expected: usize,
        /// Match count in the report.
        actual: usize,
    },
    /// A metric moved beyond the tolerance.
    #[error(
        "configuration {key} {metric}: expected {expected:.6}, got {actual:.6} (tolerance {tolerance})"
    )]
    Metric {
        /// Configuration key.
        key: String,
        /// Metric name.
        metric: &'static str,
        /// Reference value.
        expected: f64,
        /// Measured value.
        actual: f64,
        /// Allowed absolute difference.
        tolerance: f64,
    },
    /// The report contains a configuration the reference does not know.
    #[error("configuration {key} is not recorded in the reference")]
    Unrecorded {
        /// Configuration key.
        key: String,
    },
    /// The paired match count differs from the reference.
    #[error("configuration {key}: expected paired n={expected}, got paired n={actual}")]
    PairedCount {
        /// Configuration key.
        key: String,
        /// Paired count in the reference.
        expected: usize,
        /// Paired count in the report.
        actual: usize,
    },
    /// A paired metric moved beyond the tolerance.
    #[error(
        "configuration {key} {metric}: expected {expected:.6}, got {actual:.6} (tolerance {tolerance})"
    )]
    PairedMetric {
        /// Configuration key.
        key: String,
        /// Metric name.
        metric: &'static str,
        /// Reference value.
        expected: f64,
        /// Measured value.
        actual: f64,
        /// Allowed absolute difference.
        tolerance: f64,
    },
}

/// Reads the committed reference from `path`.
pub fn read_reference(path: &Path) -> Result<Reference, Error> {
    let body = fs::read_to_string(path).map_err(|source| Error::Data {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(serde_json::from_str(&body)?)
}

/// Writes the reference to `path`.
pub fn write_reference(report: &Report, path: &Path) -> Result<(), Error> {
    let mut body = serde_json::to_string_pretty(&report.reference())?;
    body.push('\n');
    fs::write(path, body).map_err(|source| Error::Data {
        path: path.to_path_buf(),
        source,
    })
}

/// Compares a report with the committed reference.
///
/// Configurations that disappeared from the report are skipped: this is what
/// lets a bookmaker vanish from the archive without failing the harness. Any
/// other difference, including a match count or a metric beyond [`TOLERANCE`],
/// is an error.
pub fn check(report: &Report, reference: &Reference) -> Result<CheckOutcome, CheckError> {
    if reference.version != REFERENCE_VERSION {
        return Err(CheckError::Version {
            found: reference.version,
            expected: REFERENCE_VERSION,
        });
    }
    if reference.from_season != EVALUATION_FROM_SEASON
        || reference.to_season != EVALUATION_TO_SEASON
    {
        return Err(CheckError::Window {
            from_season: reference.from_season,
            to_season: reference.to_season,
            expected_from: EVALUATION_FROM_SEASON,
            expected_to: EVALUATION_TO_SEASON,
        });
    }

    let mut checked = 0;
    let mut skipped = Vec::new();

    for (key, expected) in &reference.configurations {
        let Some(actual) = report.row(key) else {
            skipped.push(key.clone());
            continue;
        };
        if actual.metrics.n != expected.n {
            return Err(CheckError::Count {
                key: key.clone(),
                expected: expected.n,
                actual: actual.metrics.n,
            });
        }
        for (metric, expected, actual) in [
            ("exact", expected.exact, actual.metrics.exact),
            ("1x2", expected.one_x_two, actual.metrics.one_x_two),
            ("hit", expected.hit, actual.metrics.hit),
        ] {
            if !within_tolerance(expected, actual) {
                return Err(CheckError::Metric {
                    key: key.clone(),
                    metric,
                    expected,
                    actual,
                    tolerance: TOLERANCE,
                });
            }
        }

        if let (Some(expected), Some(actual)) = (expected.paired, actual.metrics.paired) {
            if expected.n != actual.n {
                return Err(CheckError::PairedCount {
                    key: key.clone(),
                    expected: expected.n,
                    actual: actual.n,
                });
            }
            if !within_tolerance(expected.delta, actual.delta) {
                return Err(CheckError::PairedMetric {
                    key: key.clone(),
                    metric: "delta",
                    expected: expected.delta,
                    actual: actual.delta,
                    tolerance: TOLERANCE,
                });
            }
            for (metric, expected, actual) in [
                ("ci_low", expected.ci_low, actual.ci_low),
                ("ci_high", expected.ci_high, actual.ci_high),
            ] {
                if let (Some(expected), Some(actual)) = (expected, actual) {
                    if !within_tolerance(expected, actual) {
                        return Err(CheckError::PairedMetric {
                            key: key.clone(),
                            metric,
                            expected,
                            actual,
                            tolerance: TOLERANCE,
                        });
                    }
                }
            }
        }
        checked += 1;
    }

    for row in &report.configurations {
        if !reference
            .configurations
            .contains_key(row.configuration.key())
        {
            return Err(CheckError::Unrecorded {
                key: row.configuration.key().to_owned(),
            });
        }
    }

    Ok(CheckOutcome { checked, skipped })
}

fn within_tolerance(expected: f64, actual: f64) -> bool {
    (expected - actual).abs() <= TOLERANCE
}

/// Errors returned by the backtest.
#[derive(Debug, Error)]
pub enum Error {
    /// The Football-Data client could not be built.
    #[error("failed to build the Football-Data client: {0}")]
    Client(#[source] EloError),
    /// A season could not be downloaded.
    #[error("failed to download the {season}/{} Football-Data season: {source}", season + 1)]
    Download {
        /// Start year of the season.
        season: u16,
        /// Request or status error.
        #[source]
        source: EloError,
    },
    /// A cached file could not be read or written.
    #[error("failed to access the backtest data file {}: {source}", path.display())]
    Data {
        /// File that could not be accessed.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A season file could not be parsed.
    #[error("failed to parse the {season}/{} Football-Data season: {source}", season + 1)]
    Parse {
        /// Start year of the season.
        season: u16,
        /// Parser error.
        #[source]
        source: EloError,
    },
    /// The reference file is not valid JSON for the expected schema.
    #[error("invalid reference JSON: {0}")]
    Json(#[from] serde_json::Error),
}

struct SeasonFile {
    season: u16,
    path: PathBuf,
    fingerprint: String,
    matches: Vec<SeasonMatch>,
}

/// Downloads or reads the cached seasons and measures every configuration.
pub async fn run(options: &Options) -> Result<Report, Error> {
    let files = load(options).await?;
    Ok(build_report(files))
}

async fn load(options: &Options) -> Result<Vec<SeasonFile>, Error> {
    let client = FootballDataClient::new(EloRatings::default()).map_err(Error::Client)?;
    let mut files = Vec::new();

    for season in BACKFILL_FROM_SEASON..=EVALUATION_TO_SEASON {
        let path = season_path(&options.data_dir, season);
        let body = if options.refresh || !path.exists() {
            let body = client
                .fetch_season_csv(season)
                .await
                .map_err(|source| Error::Download { season, source })?;
            write_season(&path, &body)?;
            body
        } else {
            fs::read_to_string(&path).map_err(|source| Error::Data {
                path: path.clone(),
                source,
            })?
        };
        let matches = parse_season_matches(season, &body)
            .map_err(|source| Error::Parse { season, source })?;
        files.push(SeasonFile {
            season,
            path,
            fingerprint: fingerprint(body.as_bytes()),
            matches,
        });
    }

    Ok(files)
}

fn write_season(path: &Path, body: &str) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| Error::Data {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(path, body).map_err(|source| Error::Data {
        path: path.to_path_buf(),
        source,
    })
}

fn season_path(data_dir: &Path, season: u16) -> PathBuf {
    data_dir
        .join(format!("{:02}{:02}", season % 100, (season + 1) % 100))
        .join("F1.csv")
}

fn build_report(files: Vec<SeasonFile>) -> Report {
    let mut matches: Vec<&SeasonMatch> =
        files.iter().flat_map(|file| file.matches.iter()).collect();
    matches.sort_by(|left, right| compare_matches(&left.result, &right.result));

    let mut losses: BTreeMap<Configuration, Vec<Option<MatchLoss>>> = Configuration::ALL
        .into_iter()
        .map(|configuration| (configuration, Vec::new()))
        .collect();
    let mut ratings = EloRatings::default();
    let mut evaluation: Option<DateRange> = None;
    let mut evaluation_matches = 0;

    for matched in matches {
        if in_evaluation(matched.result.season) {
            evaluation_matches += 1;
            evaluation = Some(match evaluation {
                Some(range) => DateRange {
                    from: range.from.min(matched.result.date),
                    to: range.to.max(matched.result.date),
                },
                None => DateRange {
                    from: matched.result.date,
                    to: matched.result.date,
                },
            });

            for configuration in Configuration::ALL {
                let loss = if configuration == Configuration::Elo {
                    evaluate_elo(&ratings, matched)
                } else {
                    evaluate_market(configuration, matched)
                };
                losses
                    .get_mut(&configuration)
                    .expect("every configuration is registered")
                    .push(loss);
            }
        }

        // The ratings move only after the match has been evaluated, so every
        // prediction above used the ratings as they stood before it.
        ratings.apply_results([matched.result.clone()]);
    }

    let reference = losses
        .get(&Configuration::PinnacleClosing)
        .cloned()
        .unwrap_or_default();
    let configurations = Configuration::ALL
        .into_iter()
        .filter_map(|configuration| {
            let configuration_losses = losses
                .get(&configuration)
                .expect("every configuration is registered");
            metrics_from_losses(
                configuration_losses,
                &reference,
                configuration == Configuration::PinnacleClosing,
            )
            .map(|metrics| ConfigurationReport {
                configuration,
                metrics,
            })
        })
        .collect();

    let files = files
        .into_iter()
        .map(|file| DataFileReport {
            season: file.season,
            path: file.path,
            fingerprint: file.fingerprint,
            matches: file.matches.len(),
            range: date_range(&file.matches),
        })
        .collect();

    Report {
        configurations,
        files,
        evaluation,
        evaluation_matches,
    }
}

fn compare_matches(left: &MatchResult, right: &MatchResult) -> Ordering {
    left.chronological_cmp(right)
}

fn in_evaluation(season: u16) -> bool {
    (EVALUATION_FROM_SEASON..=EVALUATION_TO_SEASON).contains(&season)
}

fn date_range(matches: &[SeasonMatch]) -> Option<DateRange> {
    let mut dates = matches.iter().map(|matched| matched.result.date);
    let first = dates.next()?;
    Some(dates.fold(
        DateRange {
            from: first,
            to: first,
        },
        |range, date| DateRange {
            from: range.from.min(date),
            to: range.to.max(date),
        },
    ))
}

fn evaluate_market(configuration: Configuration, matched: &SeasonMatch) -> Option<MatchLoss> {
    let odds = configuration.odds(&matched.odds)?;
    let (p_home, p_over) = market_probabilities(&odds)?;
    let lambdas = odds::expected_goals(p_home, p_over).ok()?;
    let prediction = model::predict_from_lambdas(lambdas, quotation(&odds)).ok()?;
    let grid = model::score_matrix(lambdas).ok()?;

    loss_from_prediction(&prediction, &grid, actual_score(&matched.result))
}

fn evaluate_elo(ratings: &EloRatings, matched: &SeasonMatch) -> Option<MatchLoss> {
    let (home, away) = (
        ratings.rating(&matched.result.home)?,
        ratings.rating(&matched.result.away)?,
    );

    let lambdas = model::lambdas(home, away).ok()?;
    let quotation = matched
        .odds
        .average_closing
        .map_or_else(neutral_quotation, |odds| quotation(&odds));
    let prediction = model::predict_from_lambdas(lambdas, quotation).ok()?;
    let grid = model::score_matrix(lambdas).ok()?;

    loss_from_prediction(&prediction, &grid, actual_score(&matched.result))
}

fn loss_from_prediction(
    prediction: &Prediction,
    grid: &[[f64; model::SCORES]; model::SCORES],
    actual: Score,
) -> Option<MatchLoss> {
    let capped = Score {
        home: actual.home.min(model::MAX_GOALS as u8),
        away: actual.away.min(model::MAX_GOALS as u8),
    };
    let exact = log_loss(grid[usize::from(capped.home)][usize::from(capped.away)])?;
    let one_x_two = log_loss(prediction.outcomes.for_outcome(Outcome::from_score(actual)))?;

    Some(MatchLoss {
        exact,
        one_x_two,
        hit: prediction.pick == actual,
    })
}

fn market_probabilities(odds: &MatchOdds) -> Option<(f64, f64)> {
    let implied_home = 1.0 / odds.home;
    let implied_draw = 1.0 / odds.draw;
    let implied_away = 1.0 / odds.away;
    let margin = implied_home + implied_draw + implied_away;

    let over = 1.0 / odds.over;
    let under = 1.0 / odds.under;
    let total = over + under;

    let p_home = implied_home / margin;
    let p_over = over / total;
    if is_probability(p_home) && is_probability(p_over) {
        Some((p_home, p_over))
    } else {
        None
    }
}

fn quotation(odds: &MatchOdds) -> Odds {
    Odds {
        home: odds.home,
        draw: odds.draw,
        away: odds.away,
    }
}

fn neutral_quotation() -> Odds {
    Odds {
        home: 1.0,
        draw: 1.0,
        away: 1.0,
    }
}

fn is_probability(value: f64) -> bool {
    value.is_finite() && value > 0.0 && value < 1.0
}

fn actual_score(result: &MatchResult) -> Score {
    Score {
        home: result.home_goals,
        away: result.away_goals,
    }
}

#[derive(Debug, Clone, Copy)]
struct MatchLoss {
    exact: f64,
    one_x_two: f64,
    hit: bool,
}

fn metrics_from_losses(
    losses: &[Option<MatchLoss>],
    reference: &[Option<MatchLoss>],
    is_reference: bool,
) -> Option<Metrics> {
    let counted: Vec<MatchLoss> = losses.iter().flatten().copied().collect();
    if counted.is_empty() {
        return None;
    }

    let n = counted.len();
    let factor = n as f64;
    let paired = if is_reference {
        Some(PairedComparison {
            delta: 0.0,
            ci_low: Some(0.0),
            ci_high: Some(0.0),
            n,
        })
    } else {
        paired_comparison(losses, reference)
    };

    Some(Metrics {
        exact: counted.iter().map(|loss| loss.exact).sum::<f64>() / factor,
        one_x_two: counted.iter().map(|loss| loss.one_x_two).sum::<f64>() / factor,
        hit: counted.iter().filter(|loss| loss.hit).count() as f64 / factor,
        n,
        paired,
    })
}

fn paired_comparison(
    losses: &[Option<MatchLoss>],
    reference: &[Option<MatchLoss>],
) -> Option<PairedComparison> {
    let differences: Vec<f64> = losses
        .iter()
        .zip(reference)
        .filter_map(|(loss, reference)| match (loss, reference) {
            (Some(loss), Some(reference)) => Some(reference.exact - loss.exact),
            _ => None,
        })
        .collect();

    if differences.is_empty() {
        return None;
    }

    let n = differences.len();
    let mean = differences.iter().sum::<f64>() / n as f64;
    let (ci_low, ci_high) = if n < 2 {
        (None, None)
    } else {
        let variance = differences
            .iter()
            .map(|difference| (difference - mean).powi(2))
            .sum::<f64>()
            / (n - 1) as f64;
        let standard_error = variance.sqrt() / (n as f64).sqrt();
        (
            Some(mean - CONFIDENCE_Z * standard_error),
            Some(mean + CONFIDENCE_Z * standard_error),
        )
    };

    Some(PairedComparison {
        delta: mean,
        ci_low,
        ci_high,
        n,
    })
}

fn format_comparison(comparison: Option<PairedComparison>) -> (String, String, String) {
    let Some(comparison) = comparison else {
        return ("—".to_owned(), "—".to_owned(), "—".to_owned());
    };
    let delta = format!("{:+.4}", comparison.delta);
    let interval = match (comparison.ci_low, comparison.ci_high) {
        (Some(low), Some(high)) => format!("[{low:+.4}, {high:+.4}]"),
        _ => "—".to_owned(),
    };
    (delta, interval, comparison.n.to_string())
}

fn log_loss(probability: f64) -> Option<f64> {
    (probability.is_finite() && probability > 0.0).then(|| -probability.ln())
}

fn fingerprint(bytes: &[u8]) -> String {
    // FNV-1a: small, deterministic and dependency-free. This is an identity
    // tag for the report, not a cryptographic hash.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\u{feff}Div,Date,Time,HomeTeam,AwayTeam,FTHG,FTAG,PSCH,PSCD,PSCA,PC>2.5,PC<2.5,AvgCH,AvgCD,AvgCA,AvgC>2.5,AvgC<2.5,MaxCH,MaxCD,MaxCA,MaxC>2.5,MaxC<2.5,PSH,PSD,PSA,P>2.5,P<2.5\r\n\
        F1,16/08/2024,20:45,Le Havre,Paris SG,1,4,7.8,5.0,1.35,1.71,2.10,7.3,4.9,1.37,1.69,2.13,8.0,5.2,1.38,1.75,2.05,7.5,5.1,1.40,1.70,2.15\r\n\
        F1,17/08/2024,20:45,Brest,Marseille,1,5,3.3,3.6,2.1,2.07,1.75,3.16,3.5,2.15,2.0,1.80,3.4,3.7,2.2,2.1,1.72,3.2,3.55,2.2,1.95,1.85\r\n\
        F1,18/08/2024,20:45,Auxerre,Nice,2,1,3.0,3.3,2.35,1.94,1.88,2.97,3.25,2.4,1.9,1.9,3.1,3.4,2.45,1.96,1.86,3.05,3.3,2.4,1.92,1.9\r\n";

    fn fixture_file(season: u16, csv: &str) -> SeasonFile {
        SeasonFile {
            season,
            path: PathBuf::from(format!("fixture-{season}.csv")),
            fingerprint: fingerprint(csv.as_bytes()),
            matches: parse_season_matches(season, csv).expect("parse fixture"),
        }
    }

    fn fixture_report() -> Report {
        build_report(vec![fixture_file(2024, FIXTURE)])
    }

    #[test]
    fn log_loss_matches_fixed_probabilities() {
        assert!((log_loss(0.5).expect("valid probability") - std::f64::consts::LN_2).abs() < 1e-12);
        assert!(
            (log_loss(0.25).expect("valid probability") - 2.0 * std::f64::consts::LN_2).abs()
                < 1e-12
        );
        assert_eq!(log_loss(0.0), None);
        assert_eq!(log_loss(f64::NAN), None);
    }

    #[test]
    fn an_embedded_fixture_traverses_the_market_pipeline() {
        let report = fixture_report();

        assert_eq!(report.evaluation_matches, 3);
        assert_eq!(report.configurations.len(), 4);
        for row in &report.configurations {
            assert_eq!(row.metrics.n, 3, "{}", row.configuration.label());
            assert!(row.metrics.exact.is_finite() && row.metrics.exact > 0.0);
            assert!(row.metrics.one_x_two.is_finite() && row.metrics.one_x_two > 0.0);
            assert!((0.0..=1.0).contains(&row.metrics.hit));
            let paired = row.metrics.paired.expect("paired against Pinnacle");
            assert_eq!(paired.n, 3);
            assert!(paired.ci_low.is_some() && paired.ci_high.is_some());
        }
        let reference = report
            .configurations
            .iter()
            .find(|row| row.configuration == Configuration::PinnacleClosing)
            .expect("reference row");
        assert_eq!(
            reference.metrics.paired.expect("self comparison").delta,
            0.0
        );
    }

    #[test]
    fn paired_interval_matches_a_hand_computed_vector() {
        // Differences reference - row: 1, 2, 3, 4.
        // mean = 2.5, sample variance = 5/3, se = sqrt(5/3)/2,
        // 1.96 * se = 1.2651745...
        let losses: Vec<Option<MatchLoss>> = [1.0, 2.0, 3.0, 4.0]
            .into_iter()
            .map(|exact| {
                Some(MatchLoss {
                    exact,
                    one_x_two: 0.0,
                    hit: false,
                })
            })
            .collect();
        let reference: Vec<Option<MatchLoss>> = [2.0, 4.0, 6.0, 8.0]
            .into_iter()
            .map(|exact| {
                Some(MatchLoss {
                    exact,
                    one_x_two: 0.0,
                    hit: false,
                })
            })
            .collect();

        let paired = paired_comparison(&losses, &reference).expect("paired comparison");

        assert_eq!(paired.n, 4);
        assert!((paired.delta - 2.5).abs() < 1e-12);
        assert!((paired.ci_low.expect("lower bound") - 1.234_825_440_238_910_5).abs() < 1e-12);
        assert!((paired.ci_high.expect("upper bound") - 3.765_174_559_761_089_5).abs() < 1e-12);
    }

    #[test]
    fn a_single_pair_has_no_interval() {
        let losses = [Some(MatchLoss {
            exact: 1.0,
            one_x_two: 0.0,
            hit: false,
        })];
        let reference = [Some(MatchLoss {
            exact: 2.0,
            one_x_two: 0.0,
            hit: false,
        })];

        let paired = paired_comparison(&losses, &reference).expect("one pair");

        assert_eq!(paired.n, 1);
        assert_eq!(paired.delta, 1.0);
        assert_eq!(paired.ci_low, None);
        assert_eq!(paired.ci_high, None);
    }

    #[test]
    fn a_source_without_columns_is_omitted() {
        let csv = "Div,Date,HomeTeam,AwayTeam,FTHG,FTAG,AvgCH,AvgCD,AvgCA,AvgC>2.5,AvgC<2.5\n\
            F1,16/08/2024,Le Havre,Paris SG,1,4,7.3,4.9,1.37,1.69,2.13\n";
        let report = build_report(vec![fixture_file(2024, csv)]);

        assert_eq!(report.configurations.len(), 1);
        assert_eq!(
            report.configurations[0].configuration,
            Configuration::AverageClosing
        );
        assert_eq!(report.configurations[0].metrics.n, 1);
        assert_eq!(report.configurations[0].metrics.paired, None);
    }

    const RESULTS_2023: &str =
        "Div,Date,HomeTeam,AwayTeam,FTHG,FTAG\nF1,12/08/2023,Paris SG,FC Nantes,2,0\n";
    const RESULTS_2024: &str =
        "Div,Date,HomeTeam,AwayTeam,FTHG,FTAG\nF1,16/08/2024,Paris SG,FC Nantes,1,0\n";

    #[test]
    fn elo_replays_the_backfill_and_uses_the_ratings_before_the_match() {
        let mut ratings = EloRatings::default();
        let previous = parse_season_matches(2023, RESULTS_2023)
            .expect("parse previous season")
            .remove(0);
        ratings.apply_results([previous.result]);

        let current = parse_season_matches(2024, RESULTS_2024)
            .expect("parse evaluated season")
            .remove(0);
        let loss = evaluate_elo(&ratings, &current).expect("one evaluated match");
        let expected = metrics_from_losses(&[Some(loss)], &[], false).expect("one evaluated match");

        let report = build_report(vec![
            fixture_file(2023, RESULTS_2023),
            fixture_file(2024, RESULTS_2024),
        ]);

        assert_eq!(report.evaluation_matches, 1);
        assert_eq!(report.configurations.len(), 1);
        assert_eq!(report.configurations[0].configuration, Configuration::Elo);
        assert_eq!(report.configurations[0].metrics, expected);
    }

    #[test]
    fn check_accepts_a_metric_within_tolerance_and_rejects_beyond_it() {
        let report = fixture_report();
        let reference = report.reference();

        let mut within = reference.clone();
        within
            .configurations
            .get_mut("average_closing")
            .expect("average closing entry")
            .exact += 5e-4;
        assert!(check(&report, &within).is_ok());

        let mut beyond = reference;
        beyond
            .configurations
            .get_mut("average_closing")
            .expect("average closing entry")
            .exact += 2e-3;
        assert!(matches!(
            check(&report, &beyond),
            Err(CheckError::Metric {
                metric: "exact",
                ..
            })
        ));
    }

    #[test]
    fn check_rejects_a_paired_delta_beyond_tolerance() {
        let report = fixture_report();
        let mut reference = report.reference();
        reference
            .configurations
            .get_mut("average_closing")
            .expect("average closing entry")
            .paired
            .as_mut()
            .expect("paired entry")
            .delta += 0.01;

        assert!(matches!(
            check(&report, &reference),
            Err(CheckError::PairedMetric {
                metric: "delta",
                ..
            })
        ));
    }

    #[test]
    fn check_rejects_a_different_match_count() {
        let report = fixture_report();
        let mut reference = report.reference();
        reference
            .configurations
            .get_mut("average_closing")
            .expect("average closing entry")
            .n += 1;

        assert!(matches!(
            check(&report, &reference),
            Err(CheckError::Count { .. })
        ));
    }

    #[test]
    fn check_skips_a_configuration_that_disappeared_from_the_report() {
        let csv = "Div,Date,HomeTeam,AwayTeam,FTHG,FTAG,AvgCH,AvgCD,AvgCA,AvgC>2.5,AvgC<2.5\n\
            F1,16/08/2024,Le Havre,Paris SG,1,4,7.3,4.9,1.37,1.69,2.13\n";
        let report = build_report(vec![fixture_file(2024, csv)]);
        let mut reference = report.reference();
        let metrics = reference.configurations["average_closing"];
        reference
            .configurations
            .insert("pinnacle_closing".to_owned(), metrics);

        let outcome = check(&report, &reference).expect("a missing source is tolerated");
        assert_eq!(outcome.checked, 1);
        assert_eq!(outcome.skipped, ["pinnacle_closing"]);
    }

    #[test]
    fn check_rejects_a_configuration_missing_from_the_reference() {
        let report = fixture_report();
        let mut reference = report.reference();
        reference.configurations.remove("pinnacle_closing");

        assert!(matches!(
            check(&report, &reference),
            Err(CheckError::Unrecorded { key }) if key == "pinnacle_closing"
        ));
    }

    #[test]
    fn the_reference_roundtrips_through_a_file() {
        let report = fixture_report();
        let path = std::env::temp_dir().join(format!(
            "mpp-autopilot-backtest-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));

        write_reference(&report, &path).expect("write reference");
        let loaded = read_reference(&path).expect("read reference");
        let _ = fs::remove_file(&path);

        let expected = report.reference();
        assert_eq!(loaded.version, expected.version);
        assert_eq!(loaded.from_season, expected.from_season);
        assert_eq!(loaded.to_season, expected.to_season);
        assert_eq!(loaded.configurations.len(), expected.configurations.len());
        for (key, expected) in &expected.configurations {
            let loaded = &loaded.configurations[key];
            assert_eq!(loaded.n, expected.n);
            assert!(within_tolerance(loaded.exact, expected.exact));
            assert!(within_tolerance(loaded.one_x_two, expected.one_x_two));
            assert!(within_tolerance(loaded.hit, expected.hit));
            match (loaded.paired, expected.paired) {
                (Some(loaded), Some(expected)) => {
                    assert_eq!(loaded.n, expected.n);
                    assert!(within_tolerance(loaded.delta, expected.delta));
                    for (loaded, expected) in [
                        (loaded.ci_low, expected.ci_low),
                        (loaded.ci_high, expected.ci_high),
                    ] {
                        match (loaded, expected) {
                            (Some(loaded), Some(expected)) => {
                                assert!(within_tolerance(loaded, expected));
                            }
                            (None, None) => {}
                            _ => panic!("CI presence differs for {key}"),
                        }
                    }
                }
                (None, None) => {}
                _ => panic!("paired presence differs for {key}"),
            }
        }
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(fingerprint(b""), "cbf29ce484222325");
        assert_eq!(fingerprint(b"hello"), "a430d84680aabd0b");
    }
}
