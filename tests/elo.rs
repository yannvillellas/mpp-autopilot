//! Public ClubElo rating index and error behavior.

use mpp_autopilot::elo::{EloError, EloRatings};
use reqwest::StatusCode;

#[test]
fn ratings_can_be_built_for_fakes() {
    let ratings = EloRatings::from_pairs([
        ("Lille OSC", 1700.0),
        ("Paris SG", 1800.0),
        ("PSG", 0.0),
        ("Lens", f64::NAN),
        ("Angers", -1.0),
        ("Nice", f64::INFINITY),
        ("Nice", 1600.0),
    ]);

    assert_eq!(ratings.len(), 3);
    assert_eq!(ratings.rating("Lille OSC"), Some(1700.0));
    assert_eq!(ratings.rating("PSG"), Some(1800.0));
    assert_eq!(ratings.rating("Nice"), Some(1600.0));
    assert_eq!(ratings.rating("Angers"), None);
    assert!(EloRatings::default().is_empty());
}

#[test]
fn duplicate_pairs_keep_the_last_value() {
    let ratings = EloRatings::from_pairs([("Paris SG", 1800.0), ("Paris SG", 1900.0)]);

    assert_eq!(ratings.rating("PSG"), Some(1900.0));
}

#[test]
fn deserialized_ratings_are_normalized_and_validated() {
    let ratings: EloRatings = serde_json::from_str(
        r#"{ " Paris-SG ": 1800.0, "Lens": -1.0, "Nice": 1600.0, "Angers": 0.0 }"#,
    )
    .expect("deserialize ratings");

    assert_eq!(ratings.len(), 2);
    assert_eq!(ratings.rating("PSG"), Some(1800.0));
    assert_eq!(ratings.rating("Nice"), Some(1600.0));
    assert_eq!(ratings.rating("Lens"), None);
    assert_eq!(ratings.rating("Angers"), None);
}

#[test]
fn transient_errors_are_classified() {
    assert!(
        EloError::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: String::new(),
        }
        .is_transient()
    );
    assert!(
        EloError::Status {
            status: StatusCode::REQUEST_TIMEOUT,
            body: String::new(),
        }
        .is_transient()
    );
    assert!(
        EloError::Status {
            status: StatusCode::TOO_MANY_REQUESTS,
            body: String::new(),
        }
        .is_transient()
    );
    assert!(
        !EloError::Status {
            status: StatusCode::BAD_REQUEST,
            body: String::new(),
        }
        .is_transient()
    );
    assert!(!EloError::NoRatings.is_transient());
    assert!(!EloError::InvalidPayload("bad payload").is_transient());
}
