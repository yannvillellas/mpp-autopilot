//! Public configuration and Auth0 token behavior.

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, TimeZone, Utc};
use mpp_autopilot::config::{Config, TokenError, token_expires_soon, token_expiry};

fn jwt(payload: &str) -> String {
    format!(
        "{}.{}.signature",
        URL_SAFE_NO_PAD.encode("{\"alg\":\"none\"}"),
        URL_SAFE_NO_PAD.encode(payload)
    )
}

fn timestamp(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, 0, 0)
        .single()
        .expect("valid timestamp")
}

#[test]
fn debug_output_redacts_the_token() {
    let config = Config {
        mpp_token: "super-secret-token".to_owned(),
        write: false,
        daily_hour: 7,
        state_path: PathBuf::from("./state.json"),
        log_level: "INFO".to_owned(),
    };
    let debug = format!("{config:?}");

    assert!(!debug.contains("super-secret-token"));
    assert!(debug.contains("[redacted]"));
}

#[test]
fn jwt_expiration_is_decoded() {
    let expires_at = timestamp(2030, 1, 1, 0);
    let token = jwt(&format!("{{\"exp\":{}}}", expires_at.timestamp()));

    assert_eq!(token_expiry(&token).expect("valid token"), expires_at);
}

#[test]
fn jwt_payload_accepts_padding() {
    let expires_at = timestamp(2030, 1, 1, 0);
    let payload = format!("{{\"exp\":{},\"pad\":\"x\"}}", expires_at.timestamp());
    let token = format!(
        "{}.{}.signature",
        URL_SAFE_NO_PAD.encode("{\"alg\":\"none\"}"),
        URL_SAFE.encode(payload)
    );

    assert_eq!(token_expiry(&token).expect("valid token"), expires_at);
}

#[test]
fn malformed_tokens_are_rejected() {
    for token in ["", "not-a-jwt", "a.b", "a.b.c.d", "a..c"] {
        assert!(
            matches!(token_expiry(token), Err(TokenError::InvalidFormat)),
            "token: {token}"
        );
    }
}

#[test]
fn invalid_payloads_are_rejected() {
    assert!(matches!(
        token_expiry(&jwt("not json")),
        Err(TokenError::InvalidPayload(_))
    ));
    assert!(matches!(
        token_expiry(&jwt("{}")),
        Err(TokenError::MissingExpiration)
    ));
    assert!(matches!(
        token_expiry(&jwt("{\"exp\":\"soon\"}")),
        Err(TokenError::MissingExpiration)
    ));
    let invalid_base64 = format!(
        "{}.{}.signature",
        URL_SAFE_NO_PAD.encode("{\"alg\":\"none\"}"),
        "not base64!"
    );
    assert!(matches!(
        token_expiry(&invalid_base64),
        Err(TokenError::InvalidBase64)
    ));
}

#[test]
fn expiration_warning_uses_a_three_day_window() {
    let now = timestamp(2026, 10, 19, 12);

    assert!(token_expires_soon(now + Duration::days(2), now));
    assert!(!token_expires_soon(now + Duration::days(3), now));
    assert!(!token_expires_soon(now + Duration::days(4), now));
    assert!(token_expires_soon(now - Duration::days(1), now));
}
