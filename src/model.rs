//! Expected-points model for exact-score predictions.
//!
//! Goal counts follow independent Poisson distributions adjusted with the
//! Dixon-Coles low-score correction. Each exact score is valued with the
//! probability of its outcome multiplied by the MPP quotation, plus a flat
//! bonus for the exact score.

use std::fmt;

use thiserror::Error;

/// Dixon-Coles low-score dependence parameter.
pub const RHO: f64 = -0.12;

/// Highest goal count considered for each team (`0..=MAX_GOALS`).
pub const MAX_GOALS: usize = 6;

/// MPP bonus points awarded for an exact score.
pub const FLAT_BONUS: f64 = 30.0;

/// Number of goal counts per team in the score grid.
pub const SCORES: usize = MAX_GOALS + 1;

/// Goals expected when the two ratings are equal.
///
/// Fitted by maximum likelihood on the 2020/21 to 2023/24 Ligue 1 seasons and
/// evaluated on 2024/25 and 2025/26, which the fit never saw. Regenerate with
/// `cargo run --features backtest --bin backtest -- --tune`. The value is
/// rounded to what the harness prints; the rounding has no measurable effect.
const BASE_TOTAL: f64 = 2.7652;

/// Extra total goals per Elo point of rating difference.
///
/// The fit learned that the total barely depends on the gap: the slope
/// collapses to about 0.00006 instead of the previous 1/2000, so the total is
/// almost flat at 2.77. Fitted on 2020/21 to 2023/24 and evaluated on 2024/25
/// and 2025/26; regenerate with
/// `cargo run --features backtest --bin backtest -- --tune`. Rounded to what
/// the harness prints; the rounding has no measurable effect.
const TOTAL_SLOPE: f64 = 0.000056;

/// Logistic scale that turns the rating difference into the home goal share.
///
/// The fitted share reacts less sharply to the rating gap than the win
/// probability (0.003833 against 0.00576): a side can win 90% of its matches,
/// but it cannot score 90% of the goals. Fitted on 2020/21 to 2023/24 and
/// evaluated on 2024/25 and 2025/26; regenerate with
/// `cargo run --features backtest --bin backtest -- --tune`. Rounded to what
/// the harness prints; the rounding has no measurable effect.
const SHARE_SCALE: f64 = 0.003833;

/// Match outcome from the home team's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Home win.
    Home,
    /// Draw.
    Draw,
    /// Away win.
    Away,
}

impl Outcome {
    /// Returns the outcome implied by an exact score.
    pub fn from_score(score: Score) -> Self {
        match score.home.cmp(&score.away) {
            std::cmp::Ordering::Greater => Self::Home,
            std::cmp::Ordering::Equal => Self::Draw,
            std::cmp::Ordering::Less => Self::Away,
        }
    }
}

/// Exact score, in goals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Score {
    /// Home goals.
    pub home: u8,
    /// Away goals.
    pub away: u8,
}

impl fmt::Display for Score {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.home, self.away)
    }
}

/// MPP quotations for the three outcomes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Odds {
    /// Quotation for a home win.
    pub home: f64,
    /// Quotation for a draw.
    pub draw: f64,
    /// Quotation for an away win.
    pub away: f64,
}

impl Odds {
    /// Returns the quotation for an outcome.
    pub fn for_outcome(&self, outcome: Outcome) -> f64 {
        match outcome {
            Outcome::Home => self.home,
            Outcome::Draw => self.draw,
            Outcome::Away => self.away,
        }
    }

    fn validate(&self) -> Result<(), ModelError> {
        let valid = self.home.is_finite()
            && self.draw.is_finite()
            && self.away.is_finite()
            && self.home > 0.0
            && self.draw > 0.0
            && self.away > 0.0;
        if valid {
            Ok(())
        } else {
            Err(ModelError::InvalidQuotations)
        }
    }
}

/// Expected goals for each team.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lambdas {
    /// Expected goals for the home team.
    pub home: f64,
    /// Expected goals for the away team.
    pub away: f64,
}

/// Probabilities of the three match outcomes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutcomeProbabilities {
    /// Probability of a home win.
    pub home: f64,
    /// Probability of a draw.
    pub draw: f64,
    /// Probability of an away win.
    pub away: f64,
}

impl OutcomeProbabilities {
    /// Returns the probability of an outcome.
    pub fn for_outcome(&self, outcome: Outcome) -> f64 {
        match outcome {
            Outcome::Home => self.home,
            Outcome::Draw => self.draw,
            Outcome::Away => self.away,
        }
    }
}

/// Complete model result for one fixture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prediction {
    /// Expected goals used by the model.
    pub lambdas: Lambdas,
    /// Probabilities of the three outcomes.
    pub outcomes: OutcomeProbabilities,
    /// Exact score maximizing the expected points.
    pub pick: Score,
    /// Expected points of [`Self::pick`].
    pub pick_ev: f64,
    /// Probability of [`Self::pick`] being the exact score.
    pub pick_probability: f64,
    /// Outcome maximizing the expected points without the exact-score bonus.
    pub recommendation: Outcome,
    /// Expected points of [`Self::recommendation`].
    pub recommendation_ev: f64,
}

/// Errors returned by the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ModelError {
    /// Elo ratings are not finite or produce invalid goal rates.
    #[error("Elo ratings must be finite and produce positive goal rates")]
    InvalidElo,
    /// Expected goals are not finite and strictly positive.
    #[error("expected goals must be finite and strictly positive")]
    InvalidLambdas,
    /// Quotations are not finite and strictly positive.
    #[error("quotations must be finite and strictly positive")]
    InvalidQuotations,
    /// The score matrix could not be normalized.
    #[error("score matrix could not be normalized")]
    Normalization,
}

/// Converts Elo ratings into expected goals.
///
/// Both ratings must be finite. The total is almost flat around the fitted
/// base total, while the split between the teams follows the fitted logistic
/// share.
pub fn lambdas(elo_home: f64, elo_away: f64) -> Result<Lambdas, ModelError> {
    if !elo_home.is_finite() || !elo_away.is_finite() {
        return Err(ModelError::InvalidElo);
    }

    let difference = elo_home - elo_away;
    let total = BASE_TOTAL + TOTAL_SLOPE * difference.abs();
    let share = 1.0 / (1.0 + (-SHARE_SCALE * difference).exp());
    let lambdas = Lambdas {
        home: total * share,
        away: total * (1.0 - share),
    };

    let valid = lambdas.home.is_finite()
        && lambdas.away.is_finite()
        && lambdas.home > 0.0
        && lambdas.away > 0.0;
    if valid {
        Ok(lambdas)
    } else {
        Err(ModelError::InvalidElo)
    }
}

/// Computes the expected-points model for one fixture from Elo ratings.
///
/// Ties are resolved in favour of the first score in row-major order
/// (home goals, then away goals) and, for the recommendation, in the order
/// home/draw/away.
pub fn predict(elo_home: f64, elo_away: f64, odds: Odds) -> Result<Prediction, ModelError> {
    odds.validate()?;
    let lambdas = lambdas(elo_home, elo_away)?;
    build_prediction(lambdas, odds)
}

/// Computes the expected-points model for one fixture from expected goals.
///
/// This is the same scoring model as [`predict`], for sources that produce
/// expected goals directly, such as market odds.
pub fn predict_from_lambdas(lambdas: Lambdas, odds: Odds) -> Result<Prediction, ModelError> {
    odds.validate()?;
    validate_lambdas(lambdas)?;
    build_prediction(lambdas, odds)
}

fn validate_lambdas(lambdas: Lambdas) -> Result<(), ModelError> {
    let valid = lambdas.home.is_finite()
        && lambdas.away.is_finite()
        && lambdas.home > 0.0
        && lambdas.away > 0.0;
    if valid {
        Ok(())
    } else {
        Err(ModelError::InvalidLambdas)
    }
}

fn build_prediction(lambdas: Lambdas, odds: Odds) -> Result<Prediction, ModelError> {
    let matrix = score_matrix(lambdas)?;
    let outcomes = outcome_probabilities(&matrix);

    let mut pick = Score { home: 0, away: 0 };
    let mut pick_ev = f64::NEG_INFINITY;
    let mut pick_probability = 0.0;

    for (home, row) in matrix.iter().enumerate() {
        for (away, &probability) in row.iter().enumerate() {
            let score = Score {
                home: home as u8,
                away: away as u8,
            };
            let outcome = Outcome::from_score(score);
            let ev = outcomes.for_outcome(outcome) * odds.for_outcome(outcome)
                + probability * FLAT_BONUS;
            if ev > pick_ev {
                pick = score;
                pick_ev = ev;
                pick_probability = probability;
            }
        }
    }

    let mut recommendation = Outcome::Home;
    let mut recommendation_ev = f64::NEG_INFINITY;
    for outcome in [Outcome::Home, Outcome::Draw, Outcome::Away] {
        let ev = outcomes.for_outcome(outcome) * odds.for_outcome(outcome);
        if ev > recommendation_ev {
            recommendation = outcome;
            recommendation_ev = ev;
        }
    }

    Ok(Prediction {
        lambdas,
        outcomes,
        pick,
        pick_ev,
        pick_probability,
        recommendation,
        recommendation_ev,
    })
}

/// Returns the normalized exact-score grid used by [`predict_from_lambdas`].
///
/// The grid is indexed by home goals and then away goals, in `0..=MAX_GOALS`.
/// Probabilities are normalized over that grid, so they sum to one.
pub fn score_matrix(lambdas: Lambdas) -> Result<[[f64; SCORES]; SCORES], ModelError> {
    validate_lambdas(lambdas)?;
    build_score_matrix(lambdas)
}

fn build_score_matrix(lambdas: Lambdas) -> Result<[[f64; SCORES]; SCORES], ModelError> {
    let mut matrix = [[0.0; SCORES]; SCORES];
    let mut total = 0.0;

    for (home, row) in matrix.iter_mut().enumerate() {
        for (away, probability) in row.iter_mut().enumerate() {
            let value = poisson(home, lambdas.home)
                * poisson(away, lambdas.away)
                * tau(home, away, lambdas);
            let value = value.clamp(0.0, 1.0);
            *probability = value;
            total += value;
        }
    }

    if !total.is_finite() || total <= 0.0 {
        return Err(ModelError::Normalization);
    }

    for row in &mut matrix {
        for probability in row {
            *probability = (*probability / total).clamp(0.0, 1.0);
        }
    }

    Ok(matrix)
}

fn outcome_probabilities(matrix: &[[f64; SCORES]; SCORES]) -> OutcomeProbabilities {
    let mut outcomes = OutcomeProbabilities {
        home: 0.0,
        draw: 0.0,
        away: 0.0,
    };

    for (home, row) in matrix.iter().enumerate() {
        for (away, probability) in row.iter().enumerate() {
            let score = Score {
                home: home as u8,
                away: away as u8,
            };
            match Outcome::from_score(score) {
                Outcome::Home => outcomes.home += probability,
                Outcome::Draw => outcomes.draw += probability,
                Outcome::Away => outcomes.away += probability,
            }
        }
    }

    outcomes
}

fn poisson(goals: usize, rate: f64) -> f64 {
    let mut probability = (-rate).exp();
    for goal in 1..=goals {
        probability *= rate / goal as f64;
    }
    probability
}

fn tau(home: usize, away: usize, lambdas: Lambdas) -> f64 {
    match (home, away) {
        (0, 0) => 1.0 - lambdas.home * lambdas.away * RHO,
        (0, 1) => 1.0 + lambdas.home * RHO,
        (1, 0) => 1.0 + lambdas.away * RHO,
        (1, 1) => 1.0 - RHO,
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_lambdas() -> Lambdas {
        lambdas(1684.0983240112262, 1670.9520303480226).expect("valid reference ratings")
    }

    #[test]
    fn score_matrix_is_normalized_and_bounded() {
        let matrix = score_matrix(reference_lambdas()).expect("valid matrix");

        let total: f64 = matrix.iter().flatten().sum();
        assert!((total - 1.0).abs() < 1e-12);
        assert!(matrix.iter().flatten().all(|p| (0.0..=1.0).contains(p)));
    }

    #[test]
    fn poisson_matches_closed_form() {
        assert!((poisson(0, 1.3) - (-1.3f64).exp()).abs() < 1e-12);
        assert!((poisson(3, 1.3) - (-1.3f64).exp() * 1.3f64.powi(3) / 6.0).abs() < 1e-12);
    }

    #[test]
    fn tau_matches_the_dixon_coles_formula() {
        let lambdas = Lambdas {
            home: 1.2,
            away: 0.9,
        };

        assert!((tau(0, 0, lambdas) - (1.0 - 1.2 * 0.9 * RHO)).abs() < 1e-12);
        assert!((tau(0, 1, lambdas) - (1.0 + 1.2 * RHO)).abs() < 1e-12);
        assert!((tau(1, 0, lambdas) - (1.0 + 0.9 * RHO)).abs() < 1e-12);
        assert!((tau(1, 1, lambdas) - (1.0 - RHO)).abs() < 1e-12);
        assert!((tau(2, 1, lambdas) - 1.0).abs() < 1e-12);
    }
}
