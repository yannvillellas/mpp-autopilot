//! Public MPP client and error behavior.

use mpp_autopilot::mpp::{MppClient, MppError};
use reqwest::StatusCode;
use serde_json::Value;

#[test]
fn client_debug_redacts_the_token() {
    let client = MppClient::new("super-secret-token").expect("build client");
    let debug = format!("{client:?}");

    assert!(!debug.contains("super-secret-token"));
    assert!(debug.contains("[redacted]"));
}

#[test]
fn transient_errors_are_classified() {
    assert!(MppError::Unauthorized.is_transient());
    assert!(
        MppError::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: String::new(),
        }
        .is_transient()
    );
    assert!(
        MppError::Status {
            status: StatusCode::TOO_MANY_REQUESTS,
            body: String::new(),
        }
        .is_transient()
    );
    assert!(
        MppError::Status {
            status: StatusCode::REQUEST_TIMEOUT,
            body: String::new(),
        }
        .is_transient()
    );
    assert!(
        !MppError::Status {
            status: StatusCode::BAD_REQUEST,
            body: String::new(),
        }
        .is_transient()
    );
    assert!(!MppError::InvalidPayload("bad payload").is_transient());

    let json_error = serde_json::from_str::<Value>("{").expect_err("invalid JSON");
    assert!(!MppError::Json(json_error).is_transient());
}

#[test]
fn kickoff_lock_is_recognized_by_status_and_body() {
    assert!(
        MppError::Status {
            status: StatusCode::FORBIDDEN,
            body: r#"{"error":"matchForecastsNotAllowed"}"#.to_owned(),
        }
        .is_kickoff_locked()
    );
    assert!(
        !MppError::Status {
            status: StatusCode::FORBIDDEN,
            body: r#"{"error":"token revoked"}"#.to_owned(),
        }
        .is_kickoff_locked()
    );
    assert!(
        !MppError::Status {
            status: StatusCode::BAD_REQUEST,
            body: "matchForecastsNotAllowed".to_owned(),
        }
        .is_kickoff_locked()
    );
    assert!(!MppError::Unauthorized.is_kickoff_locked());
}

#[test]
fn forbidden_is_recognized_from_the_status_alone() {
    assert!(
        MppError::Status {
            status: StatusCode::FORBIDDEN,
            body: String::new(),
        }
        .is_forbidden()
    );
    assert!(
        !MppError::Status {
            status: StatusCode::BAD_REQUEST,
            body: String::new(),
        }
        .is_forbidden()
    );
    assert!(!MppError::Unauthorized.is_forbidden());
}
