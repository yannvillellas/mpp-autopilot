//! Elo parameter tuning on a disjoint fit/evaluation split.
//!
//! Three conversion coefficients are fitted by maximum likelihood on the
//! 2020/21 to 2023/24 ratings. `K` and the home advantage are swept over a
//! small grid; each pair replays the whole archive from 2020/21 in memory.
//! The evaluation seasons (2024/25 and 2025/26) are never used to choose a
//! coefficient or a grid value. A split whose windows overlap is rejected
//! before any file is read.
//!
//! The fitted likelihood is the independent-Poisson likelihood of the
//! observed goals. The evaluation still runs through the production
//! Dixon-Coles score grid, with the fitted lambdas.

use std::fmt;

use thiserror::Error;

use crate::elo::{BACKFILL_FROM_SEASON, EloRatings};
use crate::model::{self, Lambdas};

use super::{
    Configuration, EVALUATION_FROM_SEASON, EVALUATION_TO_SEASON, MatchLoss, Metrics, NOISE_FLOOR,
    Options, SeasonFile, actual_score, evaluate_market, format_comparison, load,
    loss_from_prediction, metrics_from_losses, neutral_quotation,
};

/// First season used to fit the parameters (2020/21).
pub const FIT_FROM_SEASON: u16 = BACKFILL_FROM_SEASON;

/// Last season used to fit the parameters (2023/24).
pub const FIT_TO_SEASON: u16 = 2023;

/// Candidate rating-update rates.
pub const K_VALUES: [f64; 4] = [15.0, 20.0, 25.0, 30.0];

/// Candidate home advantages, in Elo points.
pub const HFA_VALUES: [f64; 4] = [45.0, 55.0, 65.0, 75.0];

const MAX_FIT_ITERATIONS: usize = 500;
const MAX_LINE_SEARCH_STEPS: usize = 40;
const GRADIENT_TOLERANCE: f64 = 1e-8;

/// Disjoint fit and evaluation windows, in season start years.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split {
    /// First fit season.
    pub fit_from: u16,
    /// Last fit season.
    pub fit_to: u16,
    /// First evaluation season.
    pub eval_from: u16,
    /// Last evaluation season.
    pub eval_to: u16,
}

impl Default for Split {
    fn default() -> Self {
        Self {
            fit_from: FIT_FROM_SEASON,
            fit_to: FIT_TO_SEASON,
            eval_from: EVALUATION_FROM_SEASON,
            eval_to: EVALUATION_TO_SEASON,
        }
    }
}

impl Split {
    /// Rejects windows that do not make sense or overlap.
    ///
    /// This is the guard the tuning entry point calls before loading data;
    /// an overlapping split is an error, not a documentation rule.
    pub fn validate(self) -> Result<(), TuneError> {
        if self.fit_from > self.fit_to {
            return Err(TuneError::InvalidFitWindow {
                from: self.fit_from,
                to: self.fit_to,
            });
        }
        if self.eval_from > self.eval_to {
            return Err(TuneError::InvalidEvaluationWindow {
                from: self.eval_from,
                to: self.eval_to,
            });
        }
        if self.fit_from <= self.eval_to && self.eval_from <= self.fit_to {
            return Err(TuneError::SplitOverlap {
                fit_from: self.fit_from,
                fit_to: self.fit_to,
                eval_from: self.eval_from,
                eval_to: self.eval_to,
            });
        }
        Ok(())
    }

    fn in_fit(self, season: u16) -> bool {
        (self.fit_from..=self.fit_to).contains(&season)
    }

    fn in_eval(self, season: u16) -> bool {
        (self.eval_from..=self.eval_to).contains(&season)
    }
}

impl fmt::Display for Split {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "fit {}/{}..{}/{}; evaluation {}/{}..{}/{}",
            self.fit_from,
            self.fit_from + 1,
            self.fit_to,
            self.fit_to + 1,
            self.eval_from,
            self.eval_from + 1,
            self.eval_to,
            self.eval_to + 1
        )
    }
}

/// Coefficients of the rating-to-goals conversion.
///
/// ```text
/// total = base_total + total_slope * |elo_home - elo_away|
/// share = logistic(share_scale * (elo_home - elo_away))
/// lambda_home = total * share
/// lambda_away = total * (1 - share)
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Conversion {
    /// Goals expected when the ratings are equal.
    pub base_total: f64,
    /// Extra total goals per Elo point of difference.
    pub total_slope: f64,
    /// Logistic scale that turns the rating difference into a goal share.
    pub share_scale: f64,
}

impl Conversion {
    /// The production conversion.
    ///
    /// These are the coefficients fitted on 2020/21 to 2023/24 and adopted by
    /// [`crate::model::lambdas`]. They are read from the model so there is a
    /// single source of truth.
    pub fn production() -> Self {
        Self {
            base_total: model::BASE_TOTAL,
            total_slope: model::TOTAL_SLOPE,
            share_scale: model::SHARE_SCALE,
        }
    }

    /// Neutral seed for the optimiser.
    ///
    /// This is the previous production conversion. It is only a starting
    /// point for the fit, never a result: keeping it away from the fitted
    /// optimum makes the iteration path reproducible across runs.
    fn starting_point() -> Self {
        Self {
            base_total: 2.5,
            total_slope: 1.0 / 2000.0,
            share_scale: 10f64.ln() / 400.0,
        }
    }

    fn lambdas(self, home_elo: f64, away_elo: f64) -> Option<Lambdas> {
        if !home_elo.is_finite() || !away_elo.is_finite() {
            return None;
        }
        let difference = home_elo - away_elo;
        let total = self.base_total + self.total_slope * difference.abs();
        if !total.is_finite() || total <= 0.0 {
            return None;
        }
        let share = 1.0 / (1.0 + (-self.share_scale * difference).exp());
        let lambdas = Lambdas {
            home: total * share,
            away: total * (1.0 - share),
        };
        (lambdas.home.is_finite()
            && lambdas.away.is_finite()
            && lambdas.home > 0.0
            && lambdas.away > 0.0)
            .then_some(lambdas)
    }

    fn logs(self) -> [f64; 3] {
        [
            self.base_total.ln(),
            self.total_slope.ln(),
            self.share_scale.ln(),
        ]
    }

    fn from_logs(logs: [f64; 3]) -> Self {
        Self {
            base_total: logs[0].exp(),
            total_slope: logs[1].exp(),
            share_scale: logs[2].exp(),
        }
    }

    fn is_valid(self) -> bool {
        self.base_total.is_finite()
            && self.total_slope.is_finite()
            && self.share_scale.is_finite()
            && self.base_total > 0.0
            && self.total_slope > 0.0
            && self.share_scale > 0.0
    }
}

const PRODUCTION_K: f64 = 20.0;
const PRODUCTION_HFA: f64 = 65.0;

/// Where a candidate's conversion comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateSource {
    /// The production conversion, evaluated without fitting.
    Production,
    /// A conversion fitted on the fit window.
    Fitted,
}

impl CandidateSource {
    fn label(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Fitted => "fitted",
        }
    }
}

/// Why a conversion fit stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitStatus {
    /// The gradient norm fell below the tolerance.
    Converged,
    /// The iteration cap was reached.
    IterationLimit,
    /// The line search could not lower the likelihood.
    LineSearchFailed,
    /// The likelihood or its gradient became non-finite.
    InvalidLikelihood,
}

impl FitStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Converged => "converged",
            Self::IterationLimit => "iter-limit",
            Self::LineSearchFailed => "line-search",
            Self::InvalidLikelihood => "invalid-nll",
        }
    }

    fn converged(self) -> bool {
        self == Self::Converged
    }
}

/// Convergence diagnostics for one fitted conversion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitDiagnostics {
    /// Number of optimisation iterations.
    pub iterations: usize,
    /// Final gradient norm, in log-coefficient space.
    pub gradient_norm: f64,
    /// Why the fit stopped.
    pub status: FitStatus,
}

/// One evaluated parameter candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct TuneCandidate {
    /// Whether the conversion is the production one or fitted.
    pub source: CandidateSource,
    /// Rating-update rate.
    pub k: f64,
    /// Home advantage, in Elo points.
    pub hfa: f64,
    /// Conversion used for the evaluation.
    pub conversion: Conversion,
    /// Fit diagnostics, absent for the production row.
    pub fit: Option<FitDiagnostics>,
    /// Metrics on the evaluation window, paired against the market.
    pub metrics: Metrics,
}

/// Result of a tuning run.
#[derive(Debug, Clone, PartialEq)]
pub struct TuneReport {
    /// Windows used by the run.
    pub split: Split,
    /// Number of fit matches the first replay produced.
    pub fit_matches: usize,
    /// Market benchmark metrics.
    pub market: Metrics,
    /// Candidates sorted by exact-score log-loss, best first.
    pub candidates: Vec<TuneCandidate>,
}

impl TuneReport {
    /// Prints the market benchmark, then the ranked candidates and their
    /// paired intervals against the market.
    pub fn print(&self) {
        println!("{split}", split = self.split);
        println!(
            "market / pinnacle closing: exact {:.4}  n={}  ({} fit matches)",
            self.market.exact, self.market.n, self.fit_matches
        );
        let best_exact = self
            .candidates
            .iter()
            .map(|candidate| candidate.metrics.exact)
            .fold(f64::INFINITY, f64::min);
        println!();
        println!(
            "{:>4} {:>4} {:>10} {:>4} {:>10} {:>3} {:>4} {:>11} {:>12} {:>12} {:>8} {:>4} {:>9} {:>22} {:>7}",
            "rank",
            "near",
            "source",
            "iter",
            "state",
            "K",
            "HFA",
            "base_total",
            "total_slope",
            "share_scale",
            "exact",
            "n",
            "delta ref",
            "95% CI",
            "paired"
        );
        for (rank, candidate) in self.candidates.iter().enumerate() {
            let (delta, interval, paired) = format_comparison(candidate.metrics.paired);
            let (iterations, state) = candidate.fit.map_or_else(
                || ("—".to_owned(), "not fitted".to_owned()),
                |fit| (fit.iterations.to_string(), fit.status.label().to_owned()),
            );
            let near = if within_noise(candidate.metrics.exact, best_exact) {
                "~"
            } else {
                ""
            };
            println!(
                "{:>4} {:>4} {:>10} {:>4} {:>10} {:>3.0} {:>4.0} {:>11.4} {:>12.6} {:>12.6} {:>8.4} {:>4} {:>9} {:>22} {:>7}",
                rank + 1,
                near,
                candidate.source.label(),
                iterations,
                state,
                candidate.k,
                candidate.hfa,
                candidate.conversion.base_total,
                candidate.conversion.total_slope,
                candidate.conversion.share_scale,
                candidate.metrics.exact,
                candidate.metrics.n,
                delta,
                interval,
                paired
            );
        }
        println!(
            "~ = exact within {NOISE_FLOOR:.2} nat of the best; the order of these rows is not a result"
        );
        let not_converged = self
            .candidates
            .iter()
            .filter(|candidate| candidate.fit.is_some_and(|fit| !fit.status.converged()))
            .count();
        if not_converged > 0 {
            println!(
                "warning: {not_converged} fitted candidate(s) did not converge; their coefficients are the last iterate"
            );
        }
        println!(
            "delta ref = market / pinnacle closing loss - candidate loss; negative means the candidate is worse"
        );
        println!(
            "noise floor: {NOISE_FLOOR:.2} nat; a difference below it, or a 95% CI containing 0, is not a result"
        );
    }
}

fn within_noise(exact: f64, best: f64) -> bool {
    exact - best < NOISE_FLOOR
}

fn check_alignment(expected: usize, actual: usize, k: f64, hfa: f64) -> Result<(), TuneError> {
    if expected == actual {
        Ok(())
    } else {
        Err(TuneError::Alignment {
            k,
            hfa,
            expected,
            actual,
        })
    }
}

/// Errors returned while tuning.
#[derive(Debug, Error)]
pub enum TuneError {
    /// The fit window is inverted.
    #[error("invalid fit window {from}/{}..{to}/{}", from + 1, to + 1)]
    InvalidFitWindow {
        /// First season.
        from: u16,
        /// Last season.
        to: u16,
    },
    /// The evaluation window is inverted.
    #[error("invalid evaluation window {from}/{}..{to}/{}", from + 1, to + 1)]
    InvalidEvaluationWindow {
        /// First season.
        from: u16,
        /// Last season.
        to: u16,
    },
    /// The fit and evaluation windows overlap.
    #[error(
        "fit window {fit_from}/{}..{fit_to}/{} overlaps evaluation window {eval_from}/{}..{eval_to}/{}",
        fit_from + 1,
        fit_to + 1,
        eval_from + 1,
        eval_to + 1
    )]
    SplitOverlap {
        /// First fit season.
        fit_from: u16,
        /// Last fit season.
        fit_to: u16,
        /// First evaluation season.
        eval_from: u16,
        /// Last evaluation season.
        eval_to: u16,
    },
    /// No completed fit match had ratings on both sides.
    #[error("the tuning run has no fit matches")]
    NoFitMatches,
    /// The market reference had no usable match.
    #[error("the tuning run has no market benchmark")]
    NoMarketBenchmark,
    /// A candidate produced no evaluation match.
    #[error("candidate K={k} HFA={hfa} produced no evaluation match")]
    NoEvaluationMatches {
        /// Rating-update rate.
        k: f64,
        /// Home advantage.
        hfa: f64,
    },
    /// The evaluation matches and the replay vectors have different lengths.
    #[error(
        "evaluation/replay alignment changed for K={k} HFA={hfa}: {expected} evaluation matches, {actual} replay entries"
    )]
    Alignment {
        /// Rating-update rate.
        k: f64,
        /// Home advantage.
        hfa: f64,
        /// Number of evaluation matches.
        expected: usize,
        /// Number of replay entries.
        actual: usize,
    },
    /// The fitted coefficients are not usable.
    #[error("the fitted conversion is not valid")]
    InvalidConversion,
    /// The seasons could not be downloaded or parsed.
    #[error("failed to load the seasons: {0}")]
    Load(#[from] super::Error),
}

/// Fits the conversion and sweeps the `K` and home-advantage grid.
pub async fn tune(options: &Options) -> Result<TuneReport, TuneError> {
    run_tune(options, Split::default()).await
}

async fn run_tune(options: &Options, split: Split) -> Result<TuneReport, TuneError> {
    split.validate()?;
    let files = load(options).await?;
    let mut matches: Vec<&super::SeasonMatch> = files
        .iter()
        .flat_map(|file: &SeasonFile| file.matches.iter())
        .collect();
    matches.sort_by(|left, right| left.result.chronological_cmp(&right.result));

    let eval_matches: Vec<&super::SeasonMatch> = matches
        .iter()
        .copied()
        .filter(|matched| split.in_eval(matched.result.season))
        .collect();
    let market_losses: Vec<Option<MatchLoss>> = eval_matches
        .iter()
        .map(|matched| evaluate_market(Configuration::PinnacleClosing, matched))
        .collect();
    let market = metrics_from_losses(&market_losses, &market_losses, true)
        .ok_or(TuneError::NoMarketBenchmark)?;

    let mut candidates = Vec::with_capacity(K_VALUES.len() * HFA_VALUES.len() + 1);
    let mut fit_matches = 0;

    for k in K_VALUES {
        for hfa in HFA_VALUES {
            let replay = replay(&matches, split, hfa, k);
            check_alignment(eval_matches.len(), replay.eval.len(), k, hfa)?;
            if replay.fit.is_empty() {
                return Err(TuneError::NoFitMatches);
            }
            fit_matches = replay.fit.len();
            let (conversion, fit) = fit_conversion(&replay.fit)?;
            let losses: Vec<Option<MatchLoss>> = eval_matches
                .iter()
                .zip(&replay.eval)
                .map(|(matched, ratings)| {
                    ratings.and_then(|ratings| evaluate_candidate(conversion, &ratings, matched))
                })
                .collect();
            let metrics = metrics_from_losses(&losses, &market_losses, false)
                .ok_or(TuneError::NoEvaluationMatches { k, hfa })?;
            candidates.push(TuneCandidate {
                source: CandidateSource::Fitted,
                k,
                hfa,
                conversion,
                fit: Some(fit),
                metrics,
            });

            if k == PRODUCTION_K && hfa == PRODUCTION_HFA {
                let production = Conversion::production();
                let production_losses: Vec<Option<MatchLoss>> = eval_matches
                    .iter()
                    .zip(&replay.eval)
                    .map(|(matched, ratings)| {
                        ratings
                            .and_then(|ratings| evaluate_candidate(production, &ratings, matched))
                    })
                    .collect();
                let production_metrics =
                    metrics_from_losses(&production_losses, &market_losses, false)
                        .ok_or(TuneError::NoEvaluationMatches { k, hfa })?;
                candidates.push(TuneCandidate {
                    source: CandidateSource::Production,
                    k,
                    hfa,
                    conversion: production,
                    fit: None,
                    metrics: production_metrics,
                });
            }
        }
    }

    candidates.sort_by(|left, right| {
        left.metrics
            .exact
            .total_cmp(&right.metrics.exact)
            .then_with(|| left.k.total_cmp(&right.k))
            .then_with(|| left.hfa.total_cmp(&right.hfa))
    });

    Ok(TuneReport {
        split,
        fit_matches,
        market,
        candidates,
    })
}

struct Replay {
    fit: Vec<RatingMatch>,
    eval: Vec<Option<RatingMatch>>,
}

#[derive(Debug, Clone, Copy)]
struct RatingMatch {
    home_elo: f64,
    away_elo: f64,
    home_goals: u8,
    away_goals: u8,
}

fn replay(matches: &[&super::SeasonMatch], split: Split, hfa: f64, k: f64) -> Replay {
    let mut ratings = EloRatings::default();
    let mut fit = Vec::new();
    let mut eval = Vec::new();

    for matched in matches {
        let snapshot = match (
            ratings.rating(&matched.result.home),
            ratings.rating(&matched.result.away),
        ) {
            (Some(home_elo), Some(away_elo)) => Some(RatingMatch {
                home_elo,
                away_elo,
                home_goals: matched.result.home_goals,
                away_goals: matched.result.away_goals,
            }),
            _ => None,
        };

        if split.in_fit(matched.result.season) {
            if let Some(snapshot) = snapshot {
                fit.push(snapshot);
            }
        }
        if split.in_eval(matched.result.season) {
            eval.push(snapshot);
        }

        ratings.apply_with(&matched.result, hfa, k);
    }

    Replay { fit, eval }
}

fn evaluate_candidate(
    conversion: Conversion,
    ratings: &RatingMatch,
    matched: &super::SeasonMatch,
) -> Option<MatchLoss> {
    let lambdas = conversion.lambdas(ratings.home_elo, ratings.away_elo)?;
    let prediction = model::predict_from_lambdas(lambdas, neutral_quotation()).ok()?;
    let grid = model::score_matrix(lambdas).ok()?;

    loss_from_prediction(&prediction, &grid, actual_score(&matched.result))
}

fn fit_conversion(matches: &[RatingMatch]) -> Result<(Conversion, FitDiagnostics), TuneError> {
    if matches.is_empty() {
        return Err(TuneError::NoFitMatches);
    }

    let mut logs = Conversion::starting_point().logs();
    let mut current = negative_log_likelihood(logs, matches);
    let mut iterations = 0;
    let mut gradient_norm = f64::NAN;
    let mut status = FitStatus::IterationLimit;

    for _ in 0..MAX_FIT_ITERATIONS {
        iterations += 1;
        let (value, gradient) = negative_log_likelihood_and_gradient(logs, matches);
        if !value.is_finite() {
            status = FitStatus::InvalidLikelihood;
            break;
        }
        let norm = gradient
            .iter()
            .map(|value| value * value)
            .sum::<f64>()
            .sqrt();
        gradient_norm = norm;
        if !norm.is_finite() {
            status = FitStatus::InvalidLikelihood;
            break;
        }
        if norm < GRADIENT_TOLERANCE {
            status = FitStatus::Converged;
            break;
        }

        let hessian = numerical_hessian(logs, matches);
        let mut direction = solve_3x3(hessian, [-gradient[0], -gradient[1], -gradient[2]])
            .unwrap_or([-gradient[0], -gradient[1], -gradient[2]]);
        let slope =
            gradient[0] * direction[0] + gradient[1] * direction[1] + gradient[2] * direction[2];
        if !slope.is_finite() || slope >= 0.0 {
            direction = [-gradient[0], -gradient[1], -gradient[2]];
        }

        let mut step = 1.0;
        let mut accepted = false;
        for _ in 0..MAX_LINE_SEARCH_STEPS {
            let candidate = std::array::from_fn(|index| logs[index] + step * direction[index]);
            let candidate_value = negative_log_likelihood(candidate, matches);
            if candidate_value < current {
                logs = candidate;
                current = candidate_value;
                accepted = true;
                break;
            }
            step *= 0.5;
        }
        if !accepted {
            status = FitStatus::LineSearchFailed;
            break;
        }
    }

    let conversion = Conversion::from_logs(logs);
    if !conversion.is_valid() {
        return Err(TuneError::InvalidConversion);
    }

    Ok((
        conversion,
        FitDiagnostics {
            iterations,
            gradient_norm,
            status,
        },
    ))
}

#[allow(
    clippy::needless_range_loop,
    reason = "the fixed 3x3 matrix indices are the operation"
)]
fn numerical_hessian(logs: [f64; 3], matches: &[RatingMatch]) -> [[f64; 3]; 3] {
    const STEP: f64 = 1e-4;
    let mut hessian = [[0.0; 3]; 3];

    for column in 0..3 {
        let mut plus = logs;
        plus[column] += STEP;
        let mut minus = logs;
        minus[column] -= STEP;
        let (_, plus_gradient) = negative_log_likelihood_and_gradient(plus, matches);
        let (_, minus_gradient) = negative_log_likelihood_and_gradient(minus, matches);
        for row in 0..3 {
            hessian[row][column] = (plus_gradient[row] - minus_gradient[row]) / (2.0 * STEP);
        }
    }

    for row in 0..3 {
        for column in row + 1..3 {
            let symmetric = (hessian[row][column] + hessian[column][row]) / 2.0;
            hessian[row][column] = symmetric;
            hessian[column][row] = symmetric;
        }
    }

    hessian
}

#[allow(
    clippy::needless_range_loop,
    reason = "the fixed 3x3 matrix indices are the operation"
)]
fn solve_3x3(mut matrix: [[f64; 3]; 3], mut rhs: [f64; 3]) -> Option<[f64; 3]> {
    for column in 0..3 {
        let mut pivot = column;
        for row in column + 1..3 {
            if matrix[row][column].abs() > matrix[pivot][column].abs() {
                pivot = row;
            }
        }
        if matrix[pivot][column].abs() < 1e-12 {
            return None;
        }
        if pivot != column {
            matrix.swap(pivot, column);
            rhs.swap(pivot, column);
        }

        let divisor = matrix[column][column];
        for entry in column..3 {
            matrix[column][entry] /= divisor;
        }
        rhs[column] /= divisor;

        for row in 0..3 {
            if row == column {
                continue;
            }
            let factor = matrix[row][column];
            for entry in column..3 {
                matrix[row][entry] -= factor * matrix[column][entry];
            }
            rhs[row] -= factor * rhs[column];
        }
    }

    rhs.iter().all(|value| value.is_finite()).then_some(rhs)
}

/// Independent-Poisson negative log-likelihood of the observed goals.
fn negative_log_likelihood(logs: [f64; 3], matches: &[RatingMatch]) -> f64 {
    let conversion = Conversion::from_logs(logs);
    let mut value = 0.0;

    for matched in matches {
        let Some(lambdas) = conversion.lambdas(matched.home_elo, matched.away_elo) else {
            return f64::INFINITY;
        };
        value += lambdas.home + lambdas.away
            - f64::from(matched.home_goals) * lambdas.home.ln()
            - f64::from(matched.away_goals) * lambdas.away.ln();
    }

    value
}

fn negative_log_likelihood_and_gradient(
    logs: [f64; 3],
    matches: &[RatingMatch],
) -> (f64, [f64; 3]) {
    let conversion = Conversion::from_logs(logs);
    let mut value = 0.0;
    let mut gradient = [0.0; 3];

    for matched in matches {
        let Some(lambdas) = conversion.lambdas(matched.home_elo, matched.away_elo) else {
            return (f64::INFINITY, [0.0; 3]);
        };
        let home_goals = f64::from(matched.home_goals);
        let away_goals = f64::from(matched.away_goals);
        value += lambdas.home + lambdas.away
            - home_goals * lambdas.home.ln()
            - away_goals * lambdas.away.ln();

        let total = lambdas.home + lambdas.away;
        let share = lambdas.home / total;
        let difference = matched.home_elo - matched.away_elo;

        let d_total = (1.0 - home_goals / lambdas.home) * share
            + (1.0 - away_goals / lambdas.away) * (1.0 - share);
        let d_share = total * (away_goals / lambdas.away - home_goals / lambdas.home);

        gradient[0] += d_total * conversion.base_total;
        gradient[1] += d_total * difference.abs() * conversion.total_slope;
        gradient[2] += d_share * share * (1.0 - share) * difference * conversion.share_scale;
    }

    (value, gradient)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg_uniform(state: &mut u64) -> f64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((*state >> 11) as f64) / ((1_u64 << 53) as f64)
    }

    fn sample_poisson(lambda: f64, state: &mut u64) -> u8 {
        let limit = (-lambda).exp();
        let mut product = 1.0;
        let mut goals = 0_u8;

        loop {
            product *= lcg_uniform(state);
            if product <= limit || goals >= 20 {
                return goals;
            }
            goals += 1;
        }
    }

    #[test]
    fn production_conversion_reproduces_model_lambdas() {
        for (home, away) in [(1_600.0, 1_600.0), (1_750.0, 1_500.0), (1_500.0, 1_750.0)] {
            let conversion = Conversion::production()
                .lambdas(home, away)
                .expect("valid production conversion");
            let model = model::lambdas(home, away).expect("valid model conversion");

            assert!((conversion.home - model.home).abs() < 1e-12);
            assert!((conversion.away - model.away).abs() < 1e-12);
        }
    }

    #[test]
    fn mismatched_evaluation_and_replay_lengths_are_an_error() {
        let error = check_alignment(3, 2, 20.0, 65.0).expect_err("mismatch");
        assert!(matches!(
            error,
            TuneError::Alignment {
                expected: 3,
                actual: 2,
                ..
            }
        ));
        assert!(check_alignment(3, 3, 20.0, 65.0).is_ok());
    }

    #[test]
    fn the_noise_marker_uses_the_configured_floor() {
        assert!(within_noise(3.035, 3.035));
        assert!(within_noise(3.044, 3.035));
        assert!(!within_noise(3.046, 3.035));
    }

    #[test]
    fn overlapping_fit_and_evaluation_windows_are_refused() {
        let split = Split {
            fit_from: 2020,
            fit_to: 2024,
            eval_from: 2024,
            eval_to: 2025,
        };

        assert!(matches!(
            split.validate(),
            Err(TuneError::SplitOverlap { .. })
        ));
    }

    #[test]
    fn the_default_split_is_disjoint() {
        assert!(Split::default().validate().is_ok());
    }

    #[tokio::test]
    async fn the_tuning_entry_point_refuses_an_overlapping_split() {
        let split = Split {
            fit_from: 2020,
            fit_to: 2024,
            eval_from: 2024,
            eval_to: 2025,
        };

        let result = run_tune(&Options::default(), split).await;

        assert!(matches!(result, Err(TuneError::SplitOverlap { .. })));
    }

    #[test]
    fn the_fit_recovers_known_coefficients_on_synthetic_goals() {
        let truth = Conversion {
            base_total: 2.4,
            total_slope: 0.0008,
            share_scale: 0.0065,
        };
        let mut state = 0x5eed_u64;
        let mut matches = Vec::new();

        for index in 0..20_000_u32 {
            let home_elo = 1_400.0 + f64::from(index % 401);
            let away_elo = 1_400.0 + f64::from((index * 7) % 401);
            let lambdas = truth.lambdas(home_elo, away_elo).expect("valid truth");
            matches.push(RatingMatch {
                home_elo,
                away_elo,
                home_goals: sample_poisson(lambdas.home, &mut state),
                away_goals: sample_poisson(lambdas.away, &mut state),
            });
        }

        let (fitted, diagnostics) = fit_conversion(&matches).expect("fit");

        assert_eq!(diagnostics.status, FitStatus::Converged);
        assert!(diagnostics.gradient_norm < GRADIENT_TOLERANCE);
        assert!(diagnostics.iterations > 0);

        assert!(
            (fitted.base_total - truth.base_total).abs() < 0.05,
            "base_total: {fitted:?}"
        );
        assert!(
            (fitted.total_slope - truth.total_slope).abs() / truth.total_slope < 0.10,
            "total_slope: {fitted:?}"
        );
        assert!(
            (fitted.share_scale - truth.share_scale).abs() / truth.share_scale < 0.05,
            "share_scale: {fitted:?}"
        );
    }
}
