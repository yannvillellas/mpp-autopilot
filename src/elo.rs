//! ClubElo ratings for the current Ligue 1 clubs.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::{Response, StatusCode};
use serde_json::Value;
use thiserror::Error;

use crate::teams;

/// ClubElo page for French clubs.
pub const CLUBELO_FRA_URL: &str = "https://clubelo.com/FRA";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_BODY_CHARS: usize = 512;

/// ClubElo ratings indexed by normalized club name.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EloRatings {
    ratings: HashMap<String, f64>,
}

impl EloRatings {
    /// Builds ratings from `(club name, Elo)` pairs.
    ///
    /// Canonical Ligue 1 names are resolved to their ClubElo spelling, other
    /// names are stored as given, and keys are normalized with
    /// [`teams::normalize_name`]. Non-finite or non-positive values are
    /// ignored. Later pairs replace earlier ones for the same normalized name.
    pub fn from_pairs<I, S>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (S, f64)>,
        S: AsRef<str>,
    {
        let mut ratings = HashMap::new();

        for (name, elo) in pairs {
            if !elo.is_finite() || elo <= 0.0 {
                continue;
            }

            let name = name.as_ref();
            let clubelo_name = teams::canonical_name(name)
                .and_then(teams::clubelo_name)
                .unwrap_or(name);
            ratings.insert(teams::normalize_name(clubelo_name), elo);
        }

        Self { ratings }
    }

    /// Returns the rating for a canonical Ligue 1 club.
    ///
    /// The canonical name is mapped through the ClubElo aliases before lookup.
    pub fn rating(&self, canonical: &str) -> Option<f64> {
        let clubelo_name = teams::clubelo_name(canonical)?;
        self.get_normalized(clubelo_name)
    }

    /// Number of clubs in the index.
    pub fn len(&self) -> usize {
        self.ratings.len()
    }

    /// Returns whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.ratings.is_empty()
    }

    fn get_normalized(&self, name: &str) -> Option<f64> {
        self.ratings.get(&teams::normalize_name(name)).copied()
    }
}

/// ClubElo client abstraction used by the orchestrator and tests.
#[allow(
    async_fn_in_trait,
    reason = "the trait is used generically with Send + Sync clients"
)]
pub trait EloSite: Send + Sync {
    /// Fetches the current French club ratings.
    async fn fetch_ratings(&self) -> Result<EloRatings, EloError>;
}

/// HTTP client for the ClubElo France page.
pub struct ClubEloClient {
    http: reqwest::Client,
}

impl ClubEloClient {
    /// Builds a client with a 30-second request timeout.
    pub fn new() -> Result<Self, EloError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(EloError::ClientBuild)?;
        Ok(Self { http })
    }
}

impl EloSite for ClubEloClient {
    async fn fetch_ratings(&self) -> Result<EloRatings, EloError> {
        let response = self
            .http
            .get(CLUBELO_FRA_URL)
            .header("accept", "text/html,application/xhtml+xml")
            .send()
            .await
            .map_err(EloError::Network)?;

        let response = check_response(response).await?;
        let body = response.text().await.map_err(EloError::Network)?;
        parse_ratings(&body)
    }
}

/// Errors returned by the ClubElo client.
#[derive(Debug, Error)]
pub enum EloError {
    /// The HTTP request failed.
    #[error("ClubElo request failed: {0}")]
    Network(#[source] reqwest::Error),
    /// The HTTP client could not be built.
    #[error("failed to build ClubElo HTTP client: {0}")]
    ClientBuild(#[source] reqwest::Error),
    /// The page returned a non-success status.
    #[error("ClubElo returned HTTP {status}: {body}")]
    Status {
        /// HTTP status.
        status: StatusCode,
        /// Truncated response body.
        body: String,
    },
    /// The page does not contain any embedded rating.
    #[error("no ClubElo ratings found in the payload")]
    NoRatings,
    /// The embedded Vega-Lite object is missing or malformed.
    #[error("ClubElo payload is invalid: {0}")]
    InvalidPayload(&'static str),
    /// The embedded Vega-Lite object is not valid JSON.
    #[error("ClubElo embedded JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
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
            Self::ClientBuild(_) | Self::NoRatings | Self::InvalidPayload(_) | Self::Json(_) => {
                false
            }
        }
    }
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

fn parse_ratings(html: &str) -> Result<EloRatings, EloError> {
    let marker = html
        .find("var vegaJson")
        .ok_or(EloError::InvalidPayload("vegaJson not found"))?;
    let offset = html[marker..]
        .find('{')
        .ok_or(EloError::InvalidPayload("vegaJson object not found"))?;
    let json_start = marker + offset;

    let value = serde_json::Deserializer::from_str(&html[json_start..])
        .into_iter::<Value>()
        .next()
        .ok_or(EloError::InvalidPayload("vegaJson is empty"))??;

    let datasets = value
        .get("datasets")
        .and_then(Value::as_object)
        .ok_or(EloError::InvalidPayload("datasets must be an object"))?;

    let mut pairs = Vec::new();
    for dataset in datasets.values() {
        let Some(rows) = dataset.as_array() else {
            continue;
        };

        for row in rows {
            let Some(elo) = row.get("Elo").and_then(Value::as_f64) else {
                continue;
            };
            let Some(name) = row.get("Name").and_then(Value::as_str) else {
                continue;
            };
            if row.get("TLC").and_then(Value::as_str).is_none() {
                continue;
            }
            pairs.push((name, elo));
        }
    }

    let ratings = EloRatings::from_pairs(pairs);
    if ratings.is_empty() {
        Err(EloError::NoRatings)
    } else {
        Ok(ratings)
    }
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
    use super::*;

    const HTML: &str = r##"
    <script>
    var vegaJson = {
      "$schema": "https://vega.github.io/schema/vega-lite/v5.20.1.json",
      "datasets": {
        "data-1": [
          {
            "Name": "Paris SG",
            "TLC": "PSG",
            "Elo": 1972.7053765140563,
            "Colour": "#ED1C24",
            "Nested": { "ignored": true }
          },
          { "Elo": 1813.2144414723184, "Name": "Lens", "TLC": "LEN" },
          { "Elo": 1684.0983240112262, "Name": "Lille", "TLC": "LIL" },
          { "Elo": 1550.5, "Name": "Le Havre", "TLC": "HAC" },
          { "Elo": 1499.0, "Name": "Auxerre", "TLC": "AJA" },
          { "Elo": 1450.0, "Name": "Le Mans", "TLC": "LEM" },
          { "Elo": 1500.0, "Name": "Saint-Étienne", "TLC": "ASSE" },
          { "Elo": 1.9e3, "Name": "Nice", "TLC": "NIC" }
        ]
      }
    };
    vegaEmbed('#chartEloGolo', vegaJson);
    </script>
    "##;

    #[test]
    fn ratings_are_indexed_by_normalized_name() {
        let ratings = parse_ratings(HTML).expect("parse ratings");

        assert_eq!(ratings.len(), 8);
        assert!(
            (ratings.get_normalized("PARIS SG").expect("PSG") - 1972.7053765140563).abs() < 1e-12
        );
        assert!(
            (ratings
                .get_normalized("Saint-Etienne")
                .expect("accent-free name")
                - 1500.0)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn canonical_aliases_resolve_to_clubelo_names() {
        let ratings = parse_ratings(HTML).expect("parse ratings");

        assert!((ratings.rating("PSG").expect("PSG") - 1972.7053765140563).abs() < 1e-12);
        assert!((ratings.rating("RC Lens").expect("Lens") - 1813.2144414723184).abs() < 1e-12);
        assert!((ratings.rating("Lille OSC").expect("Lille") - 1684.0983240112262).abs() < 1e-12);
        assert_eq!(ratings.rating("Le Havre"), Some(1550.5));
        assert_eq!(ratings.rating("AJ Auxerre"), Some(1499.0));
        assert_eq!(ratings.rating("Le Mans FC"), Some(1450.0));
        assert_eq!(ratings.rating("Nice"), Some(1900.0));
        assert_eq!(ratings.rating("Lyon"), None);
    }

    #[test]
    fn empty_payloads_are_rejected() {
        assert!(matches!(
            parse_ratings("<html></html>"),
            Err(EloError::InvalidPayload(_))
        ));
        assert!(matches!(
            parse_ratings(r#"<script>var vegaJson = { "datasets": {} };</script>"#),
            Err(EloError::NoRatings)
        ));
    }

    #[test]
    fn invalid_embedded_json_is_rejected() {
        assert!(matches!(
            parse_ratings(r#"<script>var vegaJson = { not json };</script>"#),
            Err(EloError::Json(_))
        ));
    }

    #[test]
    fn malformed_entries_do_not_cross_objects() {
        let html = r#"
            <script>
            var vegaJson = {
              "datasets": {
                "data-1": [
                  { "Elo": 100.0, "Name": "No TLC" },
                  { "Elo": 200.0, "Name": "Paris SG", "TLC": "PSG" }
                ]
              }
            };
            </script>
        "#;

        let ratings = parse_ratings(html).expect("parse ratings");

        assert_eq!(ratings.len(), 1);
        assert_eq!(ratings.rating("PSG"), Some(200.0));
    }
}
