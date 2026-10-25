//! Market odds from The Odds API v4.
//!
//! One request per trigger pass returns every upcoming Ligue 1 event with the
//! `h2h` and `totals` markets. The selected bookmaker's margin-normalised
//! 1X2 probabilities and its 2.5-goal over probability are converted to
//! expected goals, which the scoring model consumes unchanged:
//!
//! ```text
//! pOver2.5 = (1 / over) / (1 / over + 1 / under)
//! total    = lambda_home + lambda_away   (Poisson sum, 1D solve)
//! share    = lambda_home / total         (home-win probability, 1D solve)
//! ```
//!
//! Pinnacle is preferred when it has both markets; otherwise the median of
//! the complete books' normalised probabilities is used.
//!
//! The API key travels in the query string, so request errors are stripped of
//! their URL and the client's `Debug` output redacts the key.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::{Response, StatusCode};
use serde::Deserialize;
use thiserror::Error;

use crate::model::Lambdas;
use crate::teams;

/// Base URL of The Odds API v4.
pub const ODDS_API_BASE_URL: &str = "https://api.the-odds-api.com/v4";

/// Sport key of Ligue 1.
pub const LIGUE_1_SPORT_KEY: &str = "soccer_france_ligue_one";

/// Total-goals line used to pin the expected-goal sum.
pub const TOTALS_LINE: f64 = 2.5;

const REGIONS: &str = "eu";
const MARKETS: &str = "h2h,totals";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_BODY_CHARS: usize = 512;
const BISECTION_STEPS: usize = 60;

/// Expected goals derived from one Ligue 1 event's market odds.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketForecast {
    /// Kickoff returned by The Odds API.
    pub commence_time: DateTime<Utc>,
    /// Home team, with the provider spelling.
    pub home_team: String,
    /// Away team, with the provider spelling.
    pub away_team: String,
    /// Expected goals for each team.
    pub lambdas: Lambdas,
    /// Time of the odds observation used for this forecast.
    pub observed_at: DateTime<Utc>,
}

/// Market odds source used by the orchestrator and tests.
#[allow(
    async_fn_in_trait,
    reason = "the trait is used generically with Send + Sync clients"
)]
pub trait OddsSite: Send + Sync {
    /// Fetches the current odds for every upcoming Ligue 1 event.
    async fn fetch_forecasts(&self) -> Result<Vec<MarketForecast>, OddsError>;
}

/// Market source that returns no events.
///
/// Used by callers that run without a market client, such as the Elo-only
/// unit tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOdds;

impl OddsSite for NoOdds {
    async fn fetch_forecasts(&self) -> Result<Vec<MarketForecast>, OddsError> {
        Ok(Vec::new())
    }
}

/// HTTP client for The Odds API v4.
pub struct OddsApiClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl OddsApiClient {
    /// Builds a client with a 30-second request timeout.
    pub fn new(api_key: impl Into<String>) -> Result<Self, OddsError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(OddsError::ClientBuild)?;
        Ok(Self {
            http,
            base_url: ODDS_API_BASE_URL.to_owned(),
            api_key: api_key.into(),
        })
    }

    // Test-only seam. Integration tests compile the library without
    // `cfg(test)`, so the wiremock tests must stay in this module.
    #[cfg(test)]
    fn with_base_url(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, OddsError> {
        let mut client = Self::new(api_key)?;
        client.base_url = base_url.into();
        Ok(client)
    }

    fn endpoint(&self) -> String {
        format!("{}/sports/{LIGUE_1_SPORT_KEY}/odds", self.base_url)
    }

    async fn fetch(&self) -> Result<String, OddsError> {
        let response = self
            .http
            .get(self.endpoint())
            .query(&[
                ("apiKey", self.api_key.as_str()),
                ("regions", REGIONS),
                ("markets", MARKETS),
                ("oddsFormat", "decimal"),
            ])
            .send()
            .await
            .map_err(network_error)?;

        let response = check_response(response).await?;
        response.text().await.map_err(network_error)
    }
}

impl fmt::Debug for OddsApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OddsApiClient")
            .field("base_url", &self.base_url)
            .field("api_key", &"[redacted]")
            .finish()
    }
}

impl OddsSite for OddsApiClient {
    async fn fetch_forecasts(&self) -> Result<Vec<MarketForecast>, OddsError> {
        let body = self.fetch().await?;
        let observed_at = Utc::now();
        Ok(parse_events(&body)?
            .into_iter()
            .filter_map(|event| event.forecast(observed_at))
            .collect())
    }
}

/// Errors returned by the market odds client.
#[derive(Debug, Error)]
pub enum OddsError {
    /// The HTTP request failed.
    #[error("The Odds API request failed: {0}")]
    Network(#[source] reqwest::Error),
    /// The HTTP client could not be built.
    #[error("failed to build The Odds API HTTP client: {0}")]
    ClientBuild(#[source] reqwest::Error),
    /// The API returned a non-success status.
    #[error("The Odds API returned HTTP {status}: {body}")]
    Status {
        /// HTTP status.
        status: StatusCode,
        /// Truncated response body.
        body: String,
    },
    /// The response body is not valid JSON for the expected schema.
    #[error("The Odds API returned invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Errors returned while converting probabilities to expected goals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SolveError {
    /// The input probability is not strictly between 0 and 1.
    #[error("probabilities must be strictly between 0 and 1")]
    InvalidProbabilities,
    /// The one-dimensional solve did not converge.
    #[error("the expected-goal solve did not converge")]
    NoSolution,
}

/// Converts a home-win probability and a 2.5-goal over probability to
/// expected goals.
///
/// The total is solved from a Poisson goal sum; the home share is then solved
/// from the independent-Poisson home-win probability at that total.
pub fn expected_goals(p_home: f64, p_over: f64) -> Result<Lambdas, SolveError> {
    if !is_probability(p_home) || !is_probability(p_over) {
        return Err(SolveError::InvalidProbabilities);
    }

    let total = solve_total(p_over).ok_or(SolveError::NoSolution)?;
    let share = solve_share(total, p_home).ok_or(SolveError::NoSolution)?;
    let lambdas = Lambdas {
        home: total * share,
        away: total * (1.0 - share),
    };

    if lambdas.home.is_finite()
        && lambdas.away.is_finite()
        && lambdas.home > 0.0
        && lambdas.away > 0.0
    {
        Ok(lambdas)
    } else {
        Err(SolveError::NoSolution)
    }
}

fn is_probability(value: f64) -> bool {
    value.is_finite() && value > 0.0 && value < 1.0
}

fn solve_total(p_over: f64) -> Option<f64> {
    let mut low = 0.0;
    let mut high = 1.0;

    while over_probability(high) < p_over {
        high *= 2.0;
        if !high.is_finite() || high > 1_000.0 {
            return None;
        }
    }

    for _ in 0..BISECTION_STEPS {
        let mid = low + (high - low) / 2.0;
        if over_probability(mid) < p_over {
            low = mid;
        } else {
            high = mid;
        }
    }

    Some((low + high) / 2.0)
}

fn solve_share(total: f64, p_home: f64) -> Option<f64> {
    // With every goal assigned to the home side, the home team still fails
    // to win when the match ends 0-0. A target above that bound is
    // inconsistent with the total and must not be reported as a solution.
    let max_home_win = 1.0 - (-total).exp();
    if p_home >= max_home_win {
        return None;
    }

    let mut low = 0.0;
    let mut high = 1.0;

    for _ in 0..BISECTION_STEPS {
        let mid = low + (high - low) / 2.0;
        if home_win_probability(total, mid) < p_home {
            low = mid;
        } else {
            high = mid;
        }
    }

    let share = (low + high) / 2.0;
    (share > 0.0 && share < 1.0).then_some(share)
}

fn over_probability(total: f64) -> f64 {
    let at_most_two = (-total).exp() * (1.0 + total + total * total / 2.0);
    (1.0 - at_most_two).clamp(0.0, 1.0)
}

fn home_win_probability(total: f64, share: f64) -> f64 {
    let lambda_home = total * share;
    let lambda_away = total * (1.0 - share);
    let max_goals = (total + 10.0 * total.sqrt() + 40.0).ceil() as usize;

    // Recurrences for the successive Poisson masses: each iteration adds one
    // home goal and extends the away-goals CDF by one term, so the whole sum
    // is O(n) per bisection step instead of rebuilding n^2 terms.
    let mut home_pmf = (-lambda_home).exp();
    let mut away_pmf = (-lambda_away).exp();
    let mut away_lower = 0.0;
    let mut probability = 0.0;

    for home in 0..=max_goals {
        probability += home_pmf * away_lower;
        home_pmf *= lambda_home / (home + 1) as f64;
        away_lower += away_pmf;
        away_pmf *= lambda_away / (home + 1) as f64;
    }

    probability.clamp(0.0, 1.0)
}

fn network_error(error: reqwest::Error) -> OddsError {
    // The key travels in the query string; never let it reach a log line.
    OddsError::Network(error.without_url())
}

async fn check_response(response: Response) -> Result<Response, OddsError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(OddsError::Status {
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

#[derive(Debug, Deserialize)]
struct RawEvent {
    commence_time: Option<String>,
    home_team: Option<String>,
    away_team: Option<String>,
    #[serde(default)]
    bookmakers: Vec<RawBookmaker>,
}

#[derive(Debug, Deserialize)]
struct RawBookmaker {
    key: Option<String>,
    #[serde(default)]
    last_update: Option<String>,
    #[serde(default)]
    markets: Vec<RawMarket>,
}

#[derive(Debug, Deserialize)]
struct RawMarket {
    key: Option<String>,
    #[serde(default)]
    last_update: Option<String>,
    #[serde(default)]
    outcomes: Vec<RawOutcome>,
}

#[derive(Debug, Deserialize)]
struct RawOutcome {
    name: String,
    #[serde(default)]
    price: Option<f64>,
    #[serde(default)]
    point: Option<f64>,
}

struct Event {
    commence_time: DateTime<Utc>,
    home_team: String,
    away_team: String,
    books: Vec<BookOdds>,
}

#[derive(Clone, Copy)]
struct Probabilities {
    home: f64,
    draw: f64,
    away: f64,
    over: f64,
}

struct BookOdds {
    key: String,
    last_update: Option<DateTime<Utc>>,
    h2h: Option<(f64, f64, f64)>,
    totals: Option<(f64, f64)>,
}

fn parse_events(body: &str) -> Result<Vec<Event>, OddsError> {
    let raw: Vec<RawEvent> = serde_json::from_str(body)?;
    Ok(raw.into_iter().filter_map(Event::from_raw).collect())
}

impl Event {
    fn from_raw(raw: RawEvent) -> Option<Self> {
        let commence_time = parse_time(raw.commence_time.as_deref())?;
        let home_team = clean_name(raw.home_team.as_deref()?)?;
        let away_team = clean_name(raw.away_team.as_deref()?)?;
        let books: Vec<_> = raw
            .bookmakers
            .into_iter()
            .filter_map(|book| BookOdds::from_raw(book, &home_team, &away_team))
            .collect();

        Some(Self {
            commence_time,
            home_team,
            away_team,
            books,
        })
    }

    fn forecast(&self, fallback: DateTime<Utc>) -> Option<MarketForecast> {
        let (probabilities, observed_at) = self.select(fallback)?;
        let lambdas = expected_goals(probabilities.home, probabilities.over).ok()?;

        Some(MarketForecast {
            commence_time: self.commence_time,
            home_team: self.home_team.clone(),
            away_team: self.away_team.clone(),
            lambdas,
            observed_at,
        })
    }

    fn select(&self, fallback: DateTime<Utc>) -> Option<(Probabilities, DateTime<Utc>)> {
        if let Some((probabilities, observed_at)) = self
            .books
            .iter()
            .find(|book| book.key.eq_ignore_ascii_case("pinnacle"))
            .and_then(|book| book.complete(fallback))
        {
            return Some((probabilities, observed_at));
        }

        let complete: Vec<_> = self
            .books
            .iter()
            .filter_map(|book| book.complete(fallback))
            .collect();
        if complete.is_empty() {
            return None;
        }

        let probabilities = median_probabilities(complete.iter().map(|(p, _)| *p))?;
        let observed_at = complete.iter().map(|(_, at)| *at).min().unwrap_or(fallback);
        Some((probabilities, observed_at))
    }
}

impl BookOdds {
    fn from_raw(raw: RawBookmaker, home_team: &str, away_team: &str) -> Option<Self> {
        let key = raw.key?.trim().to_owned();
        if key.is_empty() {
            return None;
        }

        let mut h2h = None;
        let mut totals = None;
        let mut updates = Vec::new();

        for market in raw.markets {
            match market.key.as_deref() {
                Some("h2h") if h2h.is_none() => {
                    h2h = h2h_prices(&market, home_team, away_team);
                    if h2h.is_some() {
                        updates.push(parse_time(market.last_update.as_deref()));
                    }
                }
                Some("totals") if totals.is_none() => {
                    totals = totals_prices(&market);
                    if totals.is_some() {
                        updates.push(parse_time(market.last_update.as_deref()));
                    }
                }
                _ => {}
            }
        }

        Some(Self {
            key,
            last_update: updates
                .into_iter()
                .flatten()
                .min()
                .or_else(|| parse_time(raw.last_update.as_deref())),
            h2h,
            totals,
        })
    }

    fn complete(&self, fallback: DateTime<Utc>) -> Option<(Probabilities, DateTime<Utc>)> {
        let (home, draw, away) = self.h2h?;
        let (over, under) = self.totals?;

        let implied_home = 1.0 / home;
        let implied_draw = 1.0 / draw;
        let implied_away = 1.0 / away;
        let margin = implied_home + implied_draw + implied_away;
        if !margin.is_finite() || margin <= 0.0 {
            return None;
        }

        let over_implied = 1.0 / over;
        let under_implied = 1.0 / under;
        let over_total = over_implied + under_implied;
        if !over_total.is_finite() || over_total <= 0.0 {
            return None;
        }

        let probabilities = Probabilities {
            home: implied_home / margin,
            draw: implied_draw / margin,
            away: implied_away / margin,
            over: over_implied / over_total,
        };
        if !is_probability(probabilities.home)
            || !is_probability(probabilities.draw)
            || !is_probability(probabilities.away)
            || !is_probability(probabilities.over)
        {
            return None;
        }

        Some((probabilities, self.last_update.unwrap_or(fallback)))
    }
}

fn h2h_prices(market: &RawMarket, home_team: &str, away_team: &str) -> Option<(f64, f64, f64)> {
    let price = |target: &str| {
        market
            .outcomes
            .iter()
            .find(|outcome| teams::normalize_name(&outcome.name) == teams::normalize_name(target))
            .and_then(|outcome| outcome.price)
            .filter(|price| price.is_finite() && *price > 1.0)
    };

    Some((price(home_team)?, price("Draw")?, price(away_team)?))
}

fn totals_prices(market: &RawMarket) -> Option<(f64, f64)> {
    let price = |name: &str| {
        market
            .outcomes
            .iter()
            .find(|outcome| {
                teams::normalize_name(&outcome.name) == name
                    && outcome
                        .point
                        .is_some_and(|point| (point - TOTALS_LINE).abs() < 1e-9)
            })
            .and_then(|outcome| outcome.price)
            .filter(|price| price.is_finite() && *price > 1.0)
    };

    Some((price("over")?, price("under")?))
}

fn median_probabilities(
    probabilities: impl Iterator<Item = Probabilities>,
) -> Option<Probabilities> {
    let probabilities: Vec<_> = probabilities.collect();
    let home = median(probabilities.iter().map(|p| p.home))?;
    let draw = median(probabilities.iter().map(|p| p.draw))?;
    let away = median(probabilities.iter().map(|p| p.away))?;
    let over = median(probabilities.iter().map(|p| p.over))?;
    let margin = home + draw + away;

    if !margin.is_finite() || margin <= 0.0 || !is_probability(over) {
        return None;
    }

    let probabilities = Probabilities {
        home: home / margin,
        draw: draw / margin,
        away: away / margin,
        over,
    };
    if is_probability(probabilities.home)
        && is_probability(probabilities.draw)
        && is_probability(probabilities.away)
    {
        Some(probabilities)
    } else {
        None
    }
}

fn median(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut values: Vec<_> = values.collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    let median = if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    };
    median.is_finite().then_some(median)
}

fn parse_time(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value.trim()).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn clean_name(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORDED_RESPONSE: &str = include_str!("../tests/fixtures/ligue1_odds.json");

    fn fallback() -> DateTime<Utc> {
        "2026-10-25T17:35:00Z"
            .parse::<DateTime<Utc>>()
            .expect("valid timestamp")
    }

    fn normalised(home: f64, draw: f64, away: f64) -> (f64, f64, f64) {
        let margin = 1.0 / home + 1.0 / draw + 1.0 / away;
        (
            1.0 / home / margin,
            1.0 / draw / margin,
            1.0 / away / margin,
        )
    }

    fn over(over: f64, under: f64) -> f64 {
        (1.0 / over) / (1.0 / over + 1.0 / under)
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn recorded_response_prefers_pinnacle_and_ignores_a_book_without_totals() {
        let events = parse_events(RECORDED_RESPONSE).expect("parse recorded response");
        let forecast = events[0].forecast(fallback()).expect("Pinnacle forecast");
        let (home, _, _) = normalised(2.60, 3.51, 2.63);
        let expected = expected_goals(home, over(1.91, 1.93)).expect("solvable");

        assert_eq!(forecast.home_team, "Lorient");
        assert_eq!(forecast.away_team, "Brest");
        assert_close(forecast.lambdas.home, expected.home);
        assert_close(forecast.lambdas.away, expected.away);
        assert_eq!(
            forecast.observed_at,
            "2026-10-25T17:31:40Z"
                .parse::<DateTime<Utc>>()
                .expect("valid timestamp")
        );
    }

    #[test]
    fn recorded_response_without_pinnacle_uses_the_median_of_normalised_probabilities() {
        let events = parse_events(RECORDED_RESPONSE).expect("parse recorded response");
        let forecast = events[1].forecast(fallback()).expect("median forecast");
        let (home, _, _) = normalised(2.38, 3.56, 2.92);
        let expected = expected_goals(home, over(1.70, 2.18)).expect("solvable");

        assert_eq!(forecast.home_team, "Lille");
        assert_eq!(forecast.away_team, "RC Lens");
        assert_close(forecast.lambdas.home, expected.home);
        assert_close(forecast.lambdas.away, expected.away);
        assert_eq!(
            forecast.observed_at,
            "2026-10-25T17:31:02Z"
                .parse::<DateTime<Utc>>()
                .expect("valid timestamp")
        );
    }

    #[test]
    fn a_known_1x2_and_total_map_to_the_expected_lambdas() {
        let (home, _, _) = normalised(2.10, 3.40, 3.60);
        let lambdas = expected_goals(home, over(1.90, 1.95)).expect("solvable");

        // Reference values from the independent-Poisson bisection.
        assert!((lambdas.home - 1.5271105250436892).abs() < 1e-9);
        assert!((lambdas.away - 1.1733711983587474).abs() < 1e-9);

        let total = lambdas.home + lambdas.away;
        let share = lambdas.home / total;
        assert!((over_probability(total) - over(1.90, 1.95)).abs() < 1e-12);
        assert!((home_win_probability(total, share) - home).abs() < 1e-12);
    }

    #[test]
    fn extreme_probabilities_still_solve() {
        for (p_home, p_over) in [(0.99, 0.99), (0.01, 0.99), (0.01, 0.01)] {
            let lambdas = expected_goals(p_home, p_over).expect("solvable");
            let total = lambdas.home + lambdas.away;
            let share = lambdas.home / total;

            assert!(lambdas.home > 0.0);
            assert!(lambdas.away > 0.0);
            assert!((over_probability(total) - p_over).abs() < 1e-9);
            assert!((home_win_probability(total, share) - p_home).abs() < 1e-9);
        }
    }

    #[test]
    fn inconsistent_extreme_probabilities_are_rejected() {
        // pOver2.5 = 0.01 pins the total near 0.44, where even giving every
        // goal to the home side wins only P(total > 0) ~ 0.35 of the time.
        assert_eq!(expected_goals(0.99, 0.01), Err(SolveError::NoSolution));
    }

    #[test]
    fn invalid_probabilities_are_rejected() {
        for (p_home, p_over) in [
            (0.0, 0.5),
            (1.0, 0.5),
            (0.5, 0.0),
            (0.5, 1.0),
            (f64::NAN, 0.5),
            (0.5, f64::INFINITY),
        ] {
            assert_eq!(
                expected_goals(p_home, p_over),
                Err(SolveError::InvalidProbabilities),
                "p_home={p_home}, p_over={p_over}"
            );
        }
    }

    #[test]
    fn median_handles_even_and_odd_counts() {
        assert_eq!(median([3.0, 1.0, 2.0].into_iter()), Some(2.0));
        assert_eq!(median([4.0, 1.0, 3.0, 2.0].into_iter()), Some(2.5));
        assert_eq!(median(std::iter::empty()), None);
    }

    #[test]
    fn debug_output_redacts_the_api_key() {
        let client = OddsApiClient::new("super-secret-odds-key").expect("client");
        let debug = format!("{client:?}");

        assert!(!debug.contains("super-secret-odds-key"));
        assert!(debug.contains("[redacted]"));
    }

    #[tokio::test]
    async fn client_makes_one_request_with_the_expected_query() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v4/sports/soccer_france_ligue_one/odds"))
            .and(query_param("apiKey", "test-key"))
            .and(query_param("regions", "eu"))
            .and(query_param("markets", "h2h,totals"))
            .and(query_param("oddsFormat", "decimal"))
            .respond_with(ResponseTemplate::new(200).set_body_string(RECORDED_RESPONSE))
            .mount(&server)
            .await;

        let client = OddsApiClient::with_base_url("test-key", format!("{}/v4", server.uri()))
            .expect("client");
        let forecasts = client.fetch_forecasts().await.expect("forecasts");

        assert_eq!(forecasts.len(), 2);
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    }
}
