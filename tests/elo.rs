//! Public Elo rating index, update rule and error behavior.

use chrono::NaiveDate;
use mpp_autopilot::elo::{EloError, EloRatings, MatchResult};
use reqwest::StatusCode;

fn date(month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, month, day).expect("valid date")
}

fn result(date: NaiveDate, home: &str, away: &str, home_goals: u8, away_goals: u8) -> MatchResult {
    MatchResult {
        season: 2026,
        date,
        home: home.to_owned(),
        away: away.to_owned(),
        home_goals,
        away_goals,
    }
}

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
    assert_eq!(ratings.rating("Lens"), None);
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
fn update_rule_moves_a_known_match_by_the_expected_delta() {
    let mut ratings = EloRatings::from_pairs([("Home", 1600.0), ("Away", 1500.0)]);

    let applied = ratings.apply_results([result(date(8, 15), "Home", "Away", 2, 0)]);

    assert_eq!(applied, 1);
    let home = ratings.rating("Home").expect("home rating");
    let away = ratings.rating("Away").expect("away rating");
    assert!(
        (home - 1_608.367_652_704_073_2).abs() < 1e-9,
        "home: {home}"
    );
    assert!(
        (away - 1_491.632_347_295_926_8).abs() < 1e-9,
        "away: {away}"
    );
}

#[test]
fn a_promoted_club_starts_at_the_conventional_rating() {
    let mut ratings = EloRatings::from_pairs([("PSG", 1800.0)]);

    ratings.apply_results([result(date(8, 15), "Nantes", "PSG", 0, 0)]);

    let nantes = ratings.rating("FC Nantes").expect("promoted club rating");
    let psg = ratings.rating("PSG").expect("PSG rating");
    assert!(
        (nantes - 1_505.891_638_300_724_5).abs() < 1e-9,
        "nantes: {nantes}"
    );
    assert!((psg - 1_794.108_361_699_275_5).abs() < 1e-9, "psg: {psg}");
}

#[test]
fn out_of_order_dates_do_not_corrupt_the_sequence() {
    let first = result(date(8, 15), "Lyon", "Lille", 1, 0);
    let second = result(date(8, 22), "Lille", "Rennes", 2, 0);
    let third = result(date(8, 29), "Rennes", "Lyon", 0, 1);
    let pairs = [("Lyon", 1550.0), ("Lille", 1520.0), ("Rennes", 1500.0)];

    let mut shuffled = EloRatings::from_pairs(pairs);
    assert_eq!(
        shuffled.apply_results([third.clone(), first.clone(), second.clone()]),
        3
    );

    let mut chronological = EloRatings::from_pairs(pairs);
    for outcome in [first, second, third] {
        chronological.apply_results([outcome]);
    }

    assert_eq!(shuffled, chronological);
}

#[test]
fn applying_the_same_match_twice_changes_nothing() {
    let outcome = result(date(8, 15), "PSG", "Marseille", 1, 1);
    let mut ratings = EloRatings::from_pairs([("PSG", 1800.0), ("Marseille", 1700.0)]);

    assert_eq!(ratings.apply_results([outcome.clone(), outcome.clone()]), 1);
    let after_first = ratings.clone();

    assert_eq!(ratings.apply_results([outcome]), 0);
    assert_eq!(ratings, after_first);
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
    assert!(
        !EloError::InvalidCsv {
            season: 2020,
            reason: "missing Date column",
        }
        .is_transient()
    );
}
