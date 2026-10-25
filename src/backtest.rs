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
//! exact score; this affects only the hit column.

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
}

impl Configuration {
    /// Every configuration reported by the harness.
    pub const ALL: [Self; 4] = [
        Self::PinnacleClosing,
        Self::AverageClosing,
        Self::MaxClosing,
        Self::PinnaclePreClosing,
    ];

    /// Stable identifier used in the reference file.
    pub fn key(self) -> &'static str {
        match self {
            Self::PinnacleClosing => "pinnacle_closing",
            Self::AverageClosing => "average_closing",
            Self::MaxClosing => "max_closing",
            Self::PinnaclePreClosing => "pinnacle_pre_closing",
        }
    }

    /// Human-readable label used in the report.
    pub fn label(self) -> &'static str {
        match self {
            Self::PinnacleClosing => "market / pinnacle closing",
            Self::AverageClosing => "market / average closing",
            Self::MaxClosing => "market / best price closing",
            Self::PinnaclePreClosing => "market / pinnacle pre-closing",
        }
    }

    fn odds(self, odds: &SeasonOdds) -> Option<MatchOdds> {
        match self {
            Self::PinnacleClosing => odds.pinnacle_closing,
            Self::AverageClosing => odds.average_closing,
            Self::MaxClosing => odds.max_closing,
            Self::PinnaclePreClosing => odds.pinnacle_pre_closing,
        }
    }
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
            "{:<30} {:>8} {:>8} {:>8} {:>6}",
            "configuration", "exact", "1x2", "hit", "n"
        );
        for row in &self.configurations {
            println!(
                "{:<30} {:>8.4} {:>8.4} {:>8.4} {:>6}",
                row.configuration.label(),
                row.metrics.exact,
                row.metrics.one_x_two,
                row.metrics.hit,
                row.metrics.n
            );
        }

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

/// Errors returned by the backtest.
#[derive(Debug, Error)]
pub enum Error {
    /// The Football-Data client could not be built.
    #[error("failed to build the Football-Data client")]
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

    let mut accumulators: BTreeMap<Configuration, Accumulator> = Configuration::ALL
        .into_iter()
        .map(|configuration| (configuration, Accumulator::default()))
        .collect();
    let mut evaluation: Option<DateRange> = None;
    let mut evaluation_matches = 0;

    for matched in matches {
        if !in_evaluation(matched.result.season) {
            continue;
        }

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
            let accumulator = accumulators
                .get_mut(&configuration)
                .expect("every configuration is registered");
            evaluate_market(configuration, matched, accumulator);
        }
    }

    let configurations = Configuration::ALL
        .into_iter()
        .filter_map(|configuration| {
            let accumulator = accumulators
                .get(&configuration)
                .expect("every configuration is registered");
            accumulator.metrics().map(|metrics| ConfigurationReport {
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

fn evaluate_market(
    configuration: Configuration,
    matched: &SeasonMatch,
    accumulator: &mut Accumulator,
) {
    let Some(odds) = configuration.odds(&matched.odds) else {
        return;
    };
    let Some((p_home, p_over)) = market_probabilities(&odds) else {
        return;
    };
    let Ok(lambdas) = odds::expected_goals(p_home, p_over) else {
        return;
    };
    let Ok(prediction) = model::predict_from_lambdas(lambdas, quotation(&odds)) else {
        return;
    };
    let Ok(grid) = model::score_matrix(lambdas) else {
        return;
    };

    accumulator.record(&prediction, &grid, actual_score(&matched.result));
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

fn is_probability(value: f64) -> bool {
    value.is_finite() && value > 0.0 && value < 1.0
}

fn actual_score(result: &MatchResult) -> Score {
    Score {
        home: result.home_goals,
        away: result.away_goals,
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Accumulator {
    exact_loss: f64,
    one_x_two_loss: f64,
    hits: usize,
    n: usize,
}

impl Accumulator {
    fn record(
        &mut self,
        prediction: &Prediction,
        grid: &[[f64; model::SCORES]; model::SCORES],
        actual: Score,
    ) -> bool {
        let capped = Score {
            home: actual.home.min(model::MAX_GOALS as u8),
            away: actual.away.min(model::MAX_GOALS as u8),
        };
        let Some(exact) = log_loss(grid[usize::from(capped.home)][usize::from(capped.away)]) else {
            return false;
        };
        let Some(one_x_two) =
            log_loss(prediction.outcomes.for_outcome(Outcome::from_score(actual)))
        else {
            return false;
        };

        self.exact_loss += exact;
        self.one_x_two_loss += one_x_two;
        self.hits += usize::from(prediction.pick == actual);
        self.n += 1;
        true
    }

    fn metrics(self) -> Option<Metrics> {
        if self.n == 0 {
            return None;
        }
        let n = self.n as f64;
        Some(Metrics {
            exact: self.exact_loss / n,
            one_x_two: self.one_x_two_loss / n,
            hit: self.hits as f64 / n,
            n: self.n,
        })
    }
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

    fn fixture_report() -> Report {
        let matches = parse_season_matches(2024, FIXTURE).expect("parse fixture");
        let file = SeasonFile {
            season: 2024,
            path: PathBuf::from("fixture.csv"),
            fingerprint: fingerprint(FIXTURE.as_bytes()),
            matches,
        };
        build_report(vec![file])
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
        }
    }

    #[test]
    fn a_source_without_columns_is_omitted() {
        let csv = "Div,Date,HomeTeam,AwayTeam,FTHG,FTAG,AvgCH,AvgCD,AvgCA,AvgC>2.5,AvgC<2.5\n\
            F1,16/08/2024,Le Havre,Paris SG,1,4,7.3,4.9,1.37,1.69,2.13\n";
        let matches = parse_season_matches(2024, csv).expect("parse fixture");
        let file = SeasonFile {
            season: 2024,
            path: PathBuf::from("fixture.csv"),
            fingerprint: fingerprint(csv.as_bytes()),
            matches,
        };
        let report = build_report(vec![file]);

        assert_eq!(report.configurations.len(), 1);
        assert_eq!(
            report.configurations[0].configuration,
            Configuration::AverageClosing
        );
        assert_eq!(report.configurations[0].metrics.n, 1);
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(fingerprint(b""), "cbf29ce484222325");
        assert_eq!(fingerprint(b"hello"), "a430d84680aabd0b");
    }
}
