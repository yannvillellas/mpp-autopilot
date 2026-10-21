//! Reference-case and invariants for the expected-points model.

use mpp_autopilot::model::{Odds, Outcome, Score, lambdas, predict};

const TOLERANCE: f64 = 5e-4;

fn assert_close(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "expected {expected} +/- {tolerance}, got {actual}"
    );
}

#[test]
fn reference_case_matches_spec() {
    let odds = Odds {
        home: 81.0,
        draw: 117.0,
        away: 112.0,
    };

    let prediction = predict(1684.0983240112262, 1670.9520303480226, odds)
        .expect("the reference inputs are valid");

    assert_close(prediction.lambdas.home, 1.3007, TOLERANCE);
    assert_close(prediction.lambdas.away, 1.2059, TOLERANCE);

    assert_eq!(prediction.pick, Score { home: 1, away: 1 });
    assert_close(prediction.pick_ev, 39.4, 0.05);
    assert_close(prediction.pick_probability, 0.143, TOLERANCE);

    assert_eq!(prediction.recommendation, Outcome::Away);
}

#[test]
fn outcome_probabilities_are_normalized() {
    let prediction = predict(
        1684.0983240112262,
        1670.9520303480226,
        Odds {
            home: 81.0,
            draw: 117.0,
            away: 112.0,
        },
    )
    .expect("the reference inputs are valid");

    let total = prediction.outcomes.home + prediction.outcomes.draw + prediction.outcomes.away;
    assert_close(total, 1.0, 1e-12);

    let expected_ev = prediction.outcomes.draw * 117.0 + prediction.pick_probability * 30.0;
    assert_close(prediction.pick_ev, expected_ev, 1e-9);
}

#[test]
fn stronger_home_team_gets_higher_lambda() {
    let lambdas = lambdas(1800.0, 1500.0).expect("valid Elo ratings");

    assert!(lambdas.home > lambdas.away);
    assert!(lambdas.home > 0.0);
    assert!(lambdas.away > 0.0);
}

#[test]
fn invalid_quotations_are_rejected() {
    let result = predict(
        1600.0,
        1500.0,
        Odds {
            home: 0.0,
            draw: 117.0,
            away: 112.0,
        },
    );

    assert!(result.is_err());
}
