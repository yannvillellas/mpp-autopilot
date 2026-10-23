//! Read and write client for the private MPP API.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};
use thiserror::Error;
use tracing::warn;

use crate::model::{Odds, Score};

/// Base URL of the private MPP API.
pub const MPP_BASE_URL: &str = "https://api.mpp.football";

/// Internal MPP championship id for Ligue 1.
pub const LIGUE_1_CHAMPIONSHIP_ID: u64 = 1;

const APPLICATION: &str = "mppLfp";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const FORECAST_READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ERROR_BODY_CHARS: usize = 512;

/// Club referenced by an MPP match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Club {
    /// MPP club id.
    pub id: String,
    /// Display name resolved from `fr-FR`, `fr`, then `shortName`.
    pub name: String,
}

/// Ligue 1 fixture with positive quotations.
#[derive(Debug, Clone, PartialEq)]
pub struct Fixture {
    /// MPP match id.
    pub id: String,
    /// Game week, when provided by the API.
    pub game_week: Option<u32>,
    /// Kickoff in UTC.
    pub kickoff: DateTime<Utc>,
    /// Home/draw/away quotations.
    pub odds: Odds,
    /// Home club.
    pub home: Club,
    /// Away club.
    pub away: Club,
}

/// One daily fetch: the club directory and the Ligue 1 fixtures.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// Clubs keyed by MPP club id.
    pub clubs: HashMap<String, Club>,
    /// Ligue 1 fixtures, sorted by kickoff then match id.
    pub matches: Vec<Fixture>,
}

/// MPP client abstraction used by the orchestrator and tests.
#[allow(
    async_fn_in_trait,
    reason = "the trait is used generically with Send + Sync clients"
)]
pub trait MppApi: Send + Sync {
    /// Fetches the club directory and current Ligue 1 matches.
    async fn fetch_snapshot(&self) -> Result<Snapshot, MppError>;

    /// Submits an exact score for the `general` contest.
    async fn submit_forecast(&self, match_id: &str, score: Score) -> Result<(), MppError>;

    /// Reads back the exact score registered for the `general` contest.
    ///
    /// Returns `None` when no forecast has been submitted for the match yet.
    /// A `404` is logged as an anomaly and treated as no forecast: the
    /// observed API uses `200 {}` for an unknown match id and `400` for a
    /// malformed one. This is the source of truth for idempotence: the local
    /// state only records what the bot itself wrote.
    async fn fetch_forecast(&self, match_id: &str) -> Result<Option<Score>, MppError>;
}

/// HTTP client for the private MPP API.
pub struct MppClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl MppClient {
    /// Builds a client with a 30-second request timeout.
    pub fn new(token: impl Into<String>) -> Result<Self, MppError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MppError::ClientBuild)?;

        Ok(Self {
            http,
            base_url: MPP_BASE_URL.to_owned(),
            token: token.into(),
        })
    }

    // Test-only seam. Integration tests compile the library without
    // `cfg(test)`, so the wiremock tests must stay in this module. Do not move
    // them to `tests/` without making this constructor a deliberate public
    // API.
    #[cfg(test)]
    fn with_base_url(
        token: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, MppError> {
        let mut client = Self::new(token)?;
        client.base_url = base_url.into();
        Ok(client)
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base_url))
            .query(&[("application", APPLICATION)])
            .bearer_auth(&self.token)
            .header("application", APPLICATION)
            .header("platform", "web")
            .header("client-language", "fr-FR")
            .header("accept", "application/json")
    }

    async fn get_json(&self, path: &str) -> Result<Value, MppError> {
        let response = self
            .request(Method::GET, path)
            .send()
            .await
            .map_err(MppError::Network)?;
        let response = check_response(response).await?;
        let body = response.bytes().await.map_err(MppError::Network)?;
        Ok(serde_json::from_slice(&body)?)
    }
}

impl fmt::Debug for MppClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MppClient")
            .field("base_url", &self.base_url)
            .field("token", &"[redacted]")
            .finish()
    }
}

impl MppApi for MppClient {
    async fn fetch_snapshot(&self) -> Result<Snapshot, MppError> {
        let clubs_value = self.get_json("/championship-clubs").await?;
        let clubs = parse_clubs(&clubs_value)?;

        let matches_value = self.get_json("/championships-current-matches").await?;
        let matches = parse_matches(&matches_value, &clubs)?;

        Ok(Snapshot { clubs, matches })
    }

    async fn submit_forecast(&self, match_id: &str, score: Score) -> Result<(), MppError> {
        let path = format!("/user-match-forecasts/entity/general/match/{match_id}");
        let response = self
            .request(Method::PATCH, &path)
            .json(&forecast_body(score))
            .send()
            .await
            .map_err(MppError::Network)?;

        check_response(response).await?;
        Ok(())
    }

    async fn fetch_forecast(&self, match_id: &str) -> Result<Option<Score>, MppError> {
        let path = format!("/user-match-forecasts/{match_id}");
        let response = self
            .request(Method::GET, &path)
            .timeout(FORECAST_READ_TIMEOUT)
            .send()
            .await
            .map_err(MppError::Network)?;

        // A 404 is *not* the documented "no forecast" answer: an unknown match
        // id answers `200 {}` and a malformed one `400`, so a 404 here means a
        // wrong route, scope or gateway. It is logged before falling back to
        // writing, so a broken read route cannot disable idempotence silently.
        if response.status() == StatusCode::NOT_FOUND {
            warn!(
                match_id = %match_id,
                "MPP answered 404 to the forecast read; writing as if none were registered"
            );
            return Ok(None);
        }

        let response = check_response(response).await?;
        let body = response.bytes().await.map_err(MppError::Network)?;
        let value: Value = serde_json::from_slice(&body)?;
        parse_forecast(&value)
    }
}

/// Errors returned by the MPP client.
#[derive(Debug, Error)]
pub enum MppError {
    /// The HTTP request failed.
    #[error("MPP request failed: {0}")]
    Network(#[source] reqwest::Error),
    /// The HTTP client could not be built.
    #[error("failed to build MPP HTTP client: {0}")]
    ClientBuild(#[source] reqwest::Error),
    /// The response body is not valid JSON.
    #[error("MPP returned invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The token was rejected with HTTP 401.
    #[error("MPP authentication failed (401); renew MPP_TOKEN and restart")]
    Unauthorized,
    /// The API returned a non-success status.
    #[error("MPP returned HTTP {status}: {body}")]
    Status {
        /// HTTP status.
        status: StatusCode,
        /// Truncated response body.
        body: String,
    },
    /// The payload does not match the expected MPP schema.
    #[error("MPP payload is invalid: {0}")]
    InvalidPayload(&'static str),
}

impl MppError {
    /// Returns whether the operation may succeed later.
    ///
    /// A 401 is transient in the sense that it succeeds after the token is
    /// renewed; the service retries it with the shared backoff.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Network(_) | Self::Unauthorized => true,
            Self::Status { status, .. } => {
                status.is_server_error()
                    || matches!(
                        *status,
                        StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT
                    )
            }
            Self::ClientBuild(_) | Self::Json(_) | Self::InvalidPayload(_) => false,
        }
    }
}

async fn check_response(response: Response) -> Result<Response, MppError> {
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(MppError::Unauthorized);
    }

    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(MppError::Status {
            status,
            body: truncate_body(&body),
        });
    }

    Ok(response)
}

fn parse_clubs(value: &Value) -> Result<HashMap<String, Club>, MppError> {
    let raw_clubs = value
        .get("championshipClubs")
        .and_then(Value::as_object)
        .ok_or(MppError::InvalidPayload(
            "championshipClubs must be an object",
        ))?;

    let mut clubs = HashMap::with_capacity(raw_clubs.len());
    for (key, raw_club) in raw_clubs {
        let id = raw_club.get("id").and_then(Value::as_str).unwrap_or(key);
        let Some(name) = club_name(raw_club) else {
            warn!(club_id = %id, "skipping MPP club without a usable name");
            continue;
        };

        clubs.insert(
            id.to_owned(),
            Club {
                id: id.to_owned(),
                name: name.to_owned(),
            },
        );
    }

    Ok(clubs)
}

fn parse_matches(value: &Value, clubs: &HashMap<String, Club>) -> Result<Vec<Fixture>, MppError> {
    let raw_matches = value
        .as_object()
        .ok_or(MppError::InvalidPayload("matches must be an object"))?;

    let mut fixtures = Vec::new();

    for (key, raw_match) in raw_matches {
        if raw_match.get("championshipId").and_then(Value::as_u64) != Some(LIGUE_1_CHAMPIONSHIP_ID)
        {
            continue;
        }

        let match_id = raw_match
            .get("matchId")
            .and_then(Value::as_str)
            .unwrap_or(key);

        let Some(kickoff) = raw_match
            .get("date")
            .and_then(Value::as_str)
            .and_then(|date| DateTime::parse_from_rfc3339(date).ok())
            .map(|date| date.with_timezone(&Utc))
        else {
            warn!(match_id = %match_id, "skipping MPP match with invalid date");
            continue;
        };

        let Some(odds) = parse_odds(raw_match.get("quotations")) else {
            warn!(match_id = %match_id, "skipping MPP match without positive quotations");
            continue;
        };

        let home_id = raw_match
            .get("home")
            .and_then(|home| home.get("clubId"))
            .and_then(Value::as_str);
        let away_id = raw_match
            .get("away")
            .and_then(|away| away.get("clubId"))
            .and_then(Value::as_str);

        let (Some(home), Some(away)) = (
            home_id.and_then(|id| clubs.get(id)),
            away_id.and_then(|id| clubs.get(id)),
        ) else {
            warn!(match_id = %match_id, "skipping MPP match with an unknown club");
            continue;
        };

        fixtures.push(Fixture {
            id: match_id.to_owned(),
            game_week: raw_match
                .get("gameWeekNumber")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            kickoff,
            odds,
            home: home.clone(),
            away: away.clone(),
        });
    }

    fixtures.sort_by(|left, right| {
        left.kickoff
            .cmp(&right.kickoff)
            .then_with(|| left.id.cmp(&right.id))
    });

    Ok(fixtures)
}

fn club_name(raw_club: &Value) -> Option<&str> {
    let name = raw_club.get("name");

    name.and_then(|name| name.get("fr-FR"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            name.and_then(|name| name.get("fr"))
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            raw_club
                .get("shortName")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
        })
}

fn parse_odds(quotations: Option<&Value>) -> Option<Odds> {
    let quotations = quotations?;
    let odds = Odds {
        home: quotations.get("home")?.as_f64()?,
        draw: quotations.get("draw")?.as_f64()?,
        away: quotations.get("away")?.as_f64()?,
    };

    (odds.home > 0.0 && odds.draw > 0.0 && odds.away > 0.0).then_some(odds)
}

/// Extracts the `general` forecast from a user-match-forecasts response.
///
/// Observed shapes (probed 2026-10-23): a registered forecast answers an
/// object with `general`, an unknown match id answers `{}`, and a purged
/// forecast answers `{ "general": null }`. All of those mean "nothing to
/// compare", so the bot writes. A non-empty payload without `general` is a
/// schema change and is reported as an invalid payload, so a broken
/// assumption about the private API shows up in the logs.
fn parse_forecast(value: &Value) -> Result<Option<Score>, MppError> {
    let Some(payload) = value.as_object() else {
        return Err(MppError::InvalidPayload(
            "user-match-forecasts payload must be an object",
        ));
    };

    let Some(general) = payload.get("general") else {
        return if payload.is_empty() {
            Ok(None)
        } else {
            Err(MppError::InvalidPayload(
                "no general forecast in a non-empty payload",
            ))
        };
    };
    if general.is_null() {
        return Ok(None);
    }

    let home = general
        .get("homeScore")
        .and_then(Value::as_u64)
        .ok_or(MppError::InvalidPayload(
            "general.homeScore must be an integer",
        ))?;
    let away = general
        .get("awayScore")
        .and_then(Value::as_u64)
        .ok_or(MppError::InvalidPayload(
            "general.awayScore must be an integer",
        ))?;

    let home = u8::try_from(home)
        .map_err(|_| MppError::InvalidPayload("general.homeScore is out of range"))?;
    let away = u8::try_from(away)
        .map_err(|_| MppError::InvalidPayload("general.awayScore is out of range"))?;

    Ok(Some(Score { home, away }))
}

fn forecast_body(score: Score) -> Value {
    json!({
        "homeScore": score.home,
        "awayScore": score.away,
        "originPage": "matchSheet"
    })
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
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn clubs_json() -> Value {
        json!({
            "championshipClubs": {
                "club_psg": {
                    "id": "club_psg",
                    "name": { "fr-FR": "Paris SG", "en-GB": "Paris SG" },
                    "shortName": "PSG"
                },
                "club_om": {
                    "id": "club_om",
                    "name": { "fr": "Marseille" },
                    "shortName": "OM"
                },
                "club_fallback": {
                    "id": "club_fallback",
                    "name": { "fr-FR": null, "fr": "Nice" },
                    "shortName": "OGCN"
                },
                "club_lens": {
                    "id": "club_lens",
                    "name": {},
                    "shortName": "Lens"
                },
                "club_bad": {
                    "id": "club_bad",
                    "name": {},
                    "shortName": "  "
                }
            }
        })
    }

    fn matches_json() -> Value {
        json!({
            "m_late": {
                "matchId": "m_late",
                "championshipId": 1,
                "gameWeekNumber": 8,
                "date": "2026-10-23T18:45:00.000Z",
                "quotations": { "home": 81, "draw": 117, "away": 112 },
                "home": { "clubId": "club_psg" },
                "away": { "clubId": "club_om" }
            },
            "m_early": {
                "matchId": "m_early",
                "championshipId": 1,
                "gameWeekNumber": 8,
                "date": "2026-10-22T18:45:00.000Z",
                "quotations": { "home": 120, "draw": 110, "away": 75 },
                "home": { "clubId": "club_om" },
                "away": { "clubId": "club_lens" }
            },
            "m_other_league": {
                "matchId": "m_other_league",
                "championshipId": 2,
                "date": "2026-10-24T18:45:00.000Z",
                "quotations": { "home": 81, "draw": 117, "away": 112 },
                "home": { "clubId": "club_psg" },
                "away": { "clubId": "club_om" }
            },
            "m_zero_odds": {
                "matchId": "m_zero_odds",
                "championshipId": 1,
                "date": "2026-10-24T18:45:00.000Z",
                "quotations": { "home": 81, "draw": 117, "away": 0 },
                "home": { "clubId": "club_psg" },
                "away": { "clubId": "club_om" }
            },
            "m_bad_date": {
                "matchId": "m_bad_date",
                "championshipId": 1,
                "date": "not a date",
                "quotations": { "home": 81, "draw": 117, "away": 112 },
                "home": { "clubId": "club_psg" },
                "away": { "clubId": "club_om" }
            },
            "m_unknown_club": {
                "matchId": "m_unknown_club",
                "championshipId": 1,
                "date": "2026-10-24T18:45:00.000Z",
                "quotations": { "home": 81, "draw": 117, "away": 112 },
                "home": { "clubId": "club_missing" },
                "away": { "clubId": "club_om" }
            },
            "m_missing_odds": {
                "matchId": "m_missing_odds",
                "championshipId": 1,
                "date": "2026-10-24T18:45:00.000Z",
                "quotations": {},
                "home": { "clubId": "club_psg" },
                "away": { "clubId": "club_om" }
            }
        })
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn clubs_use_locale_then_short_name_fallback() {
        let clubs = parse_clubs(&clubs_json()).expect("parse clubs");

        assert_eq!(clubs["club_psg"].name, "Paris SG");
        assert_eq!(clubs["club_om"].name, "Marseille");
        assert_eq!(clubs["club_fallback"].name, "Nice");
        assert_eq!(clubs["club_lens"].name, "Lens");
        assert!(!clubs.contains_key("club_bad"));
    }

    #[test]
    fn matches_are_filtered_and_sorted() {
        let clubs = parse_clubs(&clubs_json()).expect("parse clubs");
        let matches = parse_matches(&matches_json(), &clubs).expect("parse matches");

        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].id, "m_early");
        assert_eq!(matches[1].id, "m_late");
        assert_eq!(matches[0].home.name, "Marseille");
        assert_eq!(matches[0].away.name, "Lens");
        assert_eq!(matches[1].home.name, "Paris SG");
        assert_eq!(matches[1].away.name, "Marseille");
        assert_eq!(matches[0].game_week, Some(8));
        assert_close(matches[1].odds.home, 81.0);
        assert_close(matches[1].odds.draw, 117.0);
        assert_close(matches[1].odds.away, 112.0);
        assert_eq!(
            matches[1].kickoff,
            "2026-10-23T18:45:00Z"
                .parse::<DateTime<Utc>>()
                .expect("valid date")
        );
    }

    #[test]
    fn invalid_top_level_payloads_are_rejected() {
        assert!(matches!(
            parse_clubs(&json!({})),
            Err(MppError::InvalidPayload(_))
        ));
        assert!(matches!(
            parse_matches(&json!([]), &HashMap::new()),
            Err(MppError::InvalidPayload(_))
        ));
    }

    #[test]
    fn forecast_payloads_are_parsed() {
        let forecast = json!({
            "general": {
                "homeScore": 0,
                "awayScore": 1,
                "editedAt": "2026-10-23T18:30:00.000Z",
                "points": null
            }
        });
        assert_eq!(
            parse_forecast(&forecast).expect("parse forecast"),
            Some(Score { home: 0, away: 1 })
        );

        assert_eq!(
            parse_forecast(&json!({ "general": null })).expect("null general"),
            None
        );
        // Observed for an unknown match id: an empty object, not a 404.
        assert_eq!(parse_forecast(&json!({})).expect("empty payload"), None);
    }

    #[test]
    fn unexpected_forecast_payloads_are_rejected() {
        // A non-empty object without `general`, an array or a bare value is a
        // schema change, not "no forecast"; it must stay visible.
        for payload in [
            json!({ "mpp_challenge_X": { "homeScore": 1 } }),
            json!([]),
            json!("nope"),
        ] {
            assert!(
                matches!(parse_forecast(&payload), Err(MppError::InvalidPayload(_))),
                "unexpected payload should be rejected: {payload}"
            );
        }

        assert!(matches!(
            parse_forecast(&json!({ "general": { "points": 12 } })),
            Err(MppError::InvalidPayload(_))
        ));
        assert!(matches!(
            parse_forecast(&json!({ "general": { "homeScore": 1 } })),
            Err(MppError::InvalidPayload(_))
        ));
        assert!(matches!(
            parse_forecast(&json!({ "general": { "homeScore": 300, "awayScore": 1 } })),
            Err(MppError::InvalidPayload(_))
        ));
    }

    #[test]
    fn forecast_body_matches_the_observed_payload() {
        let body = forecast_body(Score { home: 1, away: 1 });

        assert_eq!(body["homeScore"], 1);
        assert_eq!(body["awayScore"], 1);
        assert_eq!(body["originPage"], "matchSheet");
    }

    async fn forecast_client() -> (MockServer, MppClient) {
        let server = MockServer::start().await;
        let client = MppClient::with_base_url("token", server.uri()).expect("client");
        (server, client)
    }

    #[tokio::test]
    async fn fetch_forecast_reads_the_general_score() {
        let (server, client) = forecast_client().await;
        Mock::given(method("GET"))
            .and(path("/user-match-forecasts/match-1"))
            .and(query_param("application", "mppLfp"))
            .and(header("application", "mppLfp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "general": { "homeScore": 1, "awayScore": 0 }
            })))
            .mount(&server)
            .await;

        assert_eq!(
            client.fetch_forecast("match-1").await.expect("forecast"),
            Some(Score { home: 1, away: 0 })
        );
    }

    #[tokio::test]
    async fn fetch_forecast_treats_404_as_no_forecast() {
        let (server, client) = forecast_client().await;
        Mock::given(method("GET"))
            .and(path("/user-match-forecasts/match-1"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        assert_eq!(
            client.fetch_forecast("match-1").await.expect("forecast"),
            None
        );
    }

    #[tokio::test]
    async fn fetch_forecast_rejects_unauthorized() {
        let (server, client) = forecast_client().await;
        Mock::given(method("GET"))
            .and(path("/user-match-forecasts/match-1"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        assert!(matches!(
            client.fetch_forecast("match-1").await,
            Err(MppError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn fetch_forecast_reports_server_errors() {
        let (server, client) = forecast_client().await;
        Mock::given(method("GET"))
            .and(path("/user-match-forecasts/match-1"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        assert!(matches!(
            client.fetch_forecast("match-1").await,
            Err(MppError::Status { status, .. })
                if status == StatusCode::SERVICE_UNAVAILABLE
        ));
    }

    #[tokio::test]
    async fn fetch_forecast_rejects_invalid_json() {
        let (server, client) = forecast_client().await;
        Mock::given(method("GET"))
            .and(path("/user-match-forecasts/match-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        assert!(matches!(
            client.fetch_forecast("match-1").await,
            Err(MppError::Json(_))
        ));
    }

    #[tokio::test]
    async fn fetch_forecast_rejects_non_empty_payload_without_general() {
        let (server, client) = forecast_client().await;
        Mock::given(method("GET"))
            .and(path("/user-match-forecasts/match-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "x": 1 })))
            .mount(&server)
            .await;

        assert!(matches!(
            client.fetch_forecast("match-1").await,
            Err(MppError::InvalidPayload(
                "no general forecast in a non-empty payload"
            ))
        ));
    }
}
