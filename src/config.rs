//! Configuration parsing and Auth0 token inspection.

use std::fmt;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use thiserror::Error;

/// Default hour of the daily MPP fetch, in [`TIMEZONE`].
pub const DEFAULT_DAILY_HOUR: u8 = 7;

/// Default state file path.
pub const DEFAULT_STATE_PATH: &str = "./state.json";

/// Default log level.
pub const DEFAULT_LOG_LEVEL: &str = "INFO";

/// Timezone used for the daily schedule.
pub const TIMEZONE: &str = "Europe/Paris";

/// Warn when the MPP token expires within this many days.
pub const TOKEN_WARNING_DAYS: i64 = 3;

/// Runtime configuration loaded from the environment.
pub struct Config {
    /// Auth0 access token. Never log or print this value.
    pub mpp_token: String,
    /// Whether predictions may be written to MPP.
    pub write: bool,
    /// Hour of the daily fetch, in `Europe/Paris`.
    pub daily_hour: u8,
    /// JSON state file.
    pub state_path: PathBuf,
    /// Log level: TRACE, DEBUG, INFO, WARN or ERROR.
    pub log_level: String,
}

impl Config {
    /// Loads `.env` and then the process environment.
    ///
    /// Existing environment variables take precedence over `.env` values,
    /// and a missing `.env` file is not an error.
    pub fn load() -> Result<Self, ConfigError> {
        load_dotenv()?;
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mpp_token = optional_value(&lookup, "MPP_TOKEN").ok_or(ConfigError::MissingToken)?;

        let write = match optional_value(&lookup, "WRITE").as_deref() {
            None => false,
            Some(value) if value.eq_ignore_ascii_case("true") => true,
            Some(value) if value.eq_ignore_ascii_case("false") => false,
            Some(value) => return Err(ConfigError::InvalidWrite(value.to_owned())),
        };

        let daily_hour = match optional_value(&lookup, "DAILY_HOUR") {
            None => DEFAULT_DAILY_HOUR,
            Some(value) => value
                .parse::<u8>()
                .ok()
                .filter(|hour| *hour <= 23)
                .ok_or(ConfigError::InvalidDailyHour(value))?,
        };

        let state_path = optional_value(&lookup, "STATE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_PATH));

        let log_level =
            optional_value(&lookup, "LOG_LEVEL").unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_owned());

        Ok(Self {
            mpp_token,
            write,
            daily_hour,
            state_path,
            log_level,
        })
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("mpp_token", &"[redacted]")
            .field("write", &self.write)
            .field("daily_hour", &self.daily_hour)
            .field("state_path", &self.state_path)
            .field("log_level", &self.log_level)
            .finish()
    }
}

/// Errors raised while loading the configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// `MPP_TOKEN` is missing or blank.
    #[error("MPP_TOKEN is required")]
    MissingToken,
    /// `WRITE` is not `true` or `false`.
    #[error("WRITE must be true or false, got {0:?}")]
    InvalidWrite(String),
    /// `DAILY_HOUR` is not an hour between 0 and 23.
    #[error("DAILY_HOUR must be an integer between 0 and 23, got {0:?}")]
    InvalidDailyHour(String),
    /// `.env` exists but cannot be parsed or read.
    #[error("failed to load .env")]
    Dotenv,
}

/// Decodes the `exp` claim of a JWT without verifying its signature.
///
/// The token must have three non-empty segments. The signature is ignored.
/// The `exp` claim must be an integer Unix timestamp, as issued by Auth0.
pub fn token_expiry(token: &str) -> Result<DateTime<Utc>, TokenError> {
    let mut segments = token.trim().split('.');
    let header = segments.next();
    let payload = segments.next();
    let signature = segments.next();
    let trailing = segments.next();

    let (Some(header), Some(payload), Some(signature), None) =
        (header, payload, signature, trailing)
    else {
        return Err(TokenError::InvalidFormat);
    };

    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return Err(TokenError::InvalidFormat);
    }

    let bytes = decode_payload(payload)?;
    let claims: Value = serde_json::from_slice(&bytes)?;
    let exp = claims
        .get("exp")
        .and_then(Value::as_i64)
        .ok_or(TokenError::MissingExpiration)?;

    DateTime::<Utc>::from_timestamp(exp, 0).ok_or(TokenError::MissingExpiration)
}

/// Returns whether the token expires within [`TOKEN_WARNING_DAYS`] days.
pub fn token_expires_soon(expires_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    expires_at - now < Duration::days(TOKEN_WARNING_DAYS)
}

/// Errors raised while inspecting a JWT.
#[derive(Debug, Error)]
pub enum TokenError {
    /// The token does not have exactly three non-empty segments.
    #[error("token is not a three-segment JWT")]
    InvalidFormat,
    /// The payload is not valid base64.
    #[error("token payload is not valid base64")]
    InvalidBase64,
    /// The payload is not valid JSON.
    #[error("token payload is not valid JSON: {0}")]
    InvalidPayload(#[from] serde_json::Error),
    /// The payload has no usable `exp` claim.
    #[error("token payload has no valid exp claim")]
    MissingExpiration,
}

fn decode_payload(payload: &str) -> Result<Vec<u8>, TokenError> {
    URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .map_err(|_| TokenError::InvalidBase64)
}

fn load_dotenv() -> Result<(), ConfigError> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(error) if error.not_found() => Ok(()),
        Err(error) => Err(dotenv_error(error)),
    }
}

fn dotenv_error(_error: dotenvy::Error) -> ConfigError {
    ConfigError::Dotenv
}

fn optional_value<F>(lookup: &F, key: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(key)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        Config::from_lookup(|key| {
            vars.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn defaults_are_applied() {
        let config = config_with(&[("MPP_TOKEN", "token")]).expect("valid config");

        assert!(!config.write);
        assert_eq!(config.daily_hour, DEFAULT_DAILY_HOUR);
        assert_eq!(config.state_path, PathBuf::from(DEFAULT_STATE_PATH));
        assert_eq!(config.log_level, DEFAULT_LOG_LEVEL);
    }

    #[test]
    fn explicit_values_are_parsed() {
        let config = config_with(&[
            ("MPP_TOKEN", " token "),
            ("WRITE", "TRUE"),
            ("DAILY_HOUR", "23"),
            ("STATE_PATH", "/data/state.json"),
            ("LOG_LEVEL", "DEBUG"),
        ])
        .expect("valid config");

        assert_eq!(config.mpp_token, "token");
        assert!(config.write);
        assert_eq!(config.daily_hour, 23);
        assert_eq!(config.state_path, PathBuf::from("/data/state.json"));
        assert_eq!(config.log_level, "DEBUG");
    }

    #[test]
    fn empty_optional_values_fall_back_to_defaults() {
        let config = config_with(&[
            ("MPP_TOKEN", "token"),
            ("WRITE", ""),
            ("DAILY_HOUR", " "),
            ("STATE_PATH", ""),
            ("LOG_LEVEL", ""),
        ])
        .expect("valid config");

        assert!(!config.write);
        assert_eq!(config.daily_hour, DEFAULT_DAILY_HOUR);
        assert_eq!(config.state_path, PathBuf::from(DEFAULT_STATE_PATH));
        assert_eq!(config.log_level, DEFAULT_LOG_LEVEL);
    }

    #[test]
    fn missing_or_blank_token_is_rejected() {
        assert!(matches!(config_with(&[]), Err(ConfigError::MissingToken)));
        assert!(matches!(
            config_with(&[("MPP_TOKEN", "   ")]),
            Err(ConfigError::MissingToken)
        ));
    }

    #[test]
    fn invalid_write_is_rejected() {
        assert!(matches!(
            config_with(&[("MPP_TOKEN", "token"), ("WRITE", "yes")]),
            Err(ConfigError::InvalidWrite(value)) if value == "yes"
        ));
    }

    #[test]
    fn invalid_daily_hour_is_rejected() {
        for value in ["24", "-1", "abc", "7.5"] {
            assert!(
                matches!(
                    config_with(&[("MPP_TOKEN", "token"), ("DAILY_HOUR", value)]),
                    Err(ConfigError::InvalidDailyHour(invalid)) if invalid == value
                ),
                "value: {value}"
            );
        }
    }

    #[test]
    fn midnight_is_a_valid_daily_hour() {
        let config =
            config_with(&[("MPP_TOKEN", "token"), ("DAILY_HOUR", "0")]).expect("valid config");

        assert_eq!(config.daily_hour, 0);
    }

    #[test]
    fn dotenv_parse_errors_do_not_expose_secrets() {
        let secret = "eyJhbGciOiJIUzI1NiJ9.secret";
        let error = dotenv_error(dotenvy::Error::LineParse(
            format!("MPP_TOKEN=\"{secret}"),
            4,
        ));

        let display = format!("{error}");
        let debug = format!("{error:?}");

        assert!(!display.contains(secret));
        assert!(!debug.contains(secret));
    }
}
