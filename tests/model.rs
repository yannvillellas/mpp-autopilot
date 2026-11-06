//! Reference-case and invariants for the expected-points model.

use mpp_autopilot::model::{
    Lambdas, ModelError, Odds, Outcome, Score, lambdas, points, predict, predict_from_lambdas,
    score_matrix,
};

const TOLERANCE: f64 = 5e-4;

fn assert_close(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "expected {expected} +/- {tolerance}, got {actual}"
    );
}

#[test]
fn reference_case_matches_expected_values() {
    let odds = Odds {
        home: 81.0,
        draw: 117.0,
        away: 112.0,
    };

    let prediction = predict(1684.0983240112262, 1670.9520303480226, odds)
        .expect("the reference inputs are valid");

    assert_close(prediction.lambdas.home, 1.4178, TOLERANCE);
    assert_close(prediction.lambdas.away, 1.3481, TOLERANCE);

    assert_eq!(prediction.pick, Score { home: 2, away: 1 });
    assert_close(prediction.pick_ev, 30.295102, TOLERANCE);
    assert_close(prediction.pick_probability, 0.085352, TOLERANCE);

    assert_eq!(prediction.recommendation, Outcome::Home);
}

#[test]
fn fitted_conversion_matches_the_tuned_values_and_is_symmetric() {
    let cases = [
        (1500.0, 1500.0, 1.382600, 1.382600),
        (1700.0, 1500.0, 1.895684, 0.880716),
        (1500.0, 1700.0, 0.880716, 1.895684),
    ];

    for (home_elo, away_elo, home, away) in cases {
        let lambdas = lambdas(home_elo, away_elo).expect("valid Elo ratings");

        assert_close(lambdas.home, home, 1e-6);
        assert_close(lambdas.away, away, 1e-6);
    }
}

#[test]
fn outcome_probabilities_are_normalized() {
    let odds = Odds {
        home: 81.0,
        draw: 117.0,
        away: 112.0,
    };
    let prediction = predict(1684.0983240112262, 1670.9520303480226, odds)
        .expect("the reference inputs are valid");

    let total = prediction.outcomes.home + prediction.outcomes.draw + prediction.outcomes.away;
    assert_close(total, 1.0, 1e-12);
}

#[test]
fn pick_is_the_most_probable_score_of_the_recommended_outcome() {
    let odds = Odds {
        home: 81.0,
        draw: 117.0,
        away: 112.0,
    };
    let prediction = predict(1684.0983240112262, 1670.9520303480226, odds)
        .expect("the reference inputs are valid");
    let matrix = score_matrix(prediction.lambdas).expect("score matrix");

    assert_eq!(
        Outcome::from_score(prediction.pick),
        prediction.recommendation
    );
    assert_eq!(prediction.pick, Score { home: 2, away: 1 });
    assert_eq!(prediction.modal, Score { home: 1, away: 1 });
    assert_ne!(prediction.pick, prediction.modal);

    let best_in_outcome = matrix
        .iter()
        .enumerate()
        .flat_map(|(home, row)| {
            row.iter().enumerate().map(move |(away, &probability)| {
                (
                    Score {
                        home: home as u8,
                        away: away as u8,
                    },
                    probability,
                )
            })
        })
        .filter(|(score, _)| Outcome::from_score(*score) == prediction.recommendation)
        .map(|(_, probability)| probability)
        .fold(f64::NEG_INFINITY, f64::max);
    let pick_probability =
        matrix[usize::from(prediction.pick.home)][usize::from(prediction.pick.away)];
    assert_close(pick_probability, best_in_outcome, 1e-12);
    assert_close(prediction.pick_probability, pick_probability, 1e-12);

    let expected_ev = prediction.outcomes.for_outcome(prediction.recommendation)
        * odds.for_outcome(prediction.recommendation);
    assert_close(prediction.pick_ev, expected_ev, 1e-9);
}

#[test]
fn recommendation_follows_the_lowest_quotation() {
    let expected_lambdas =
        lambdas(1684.0983240112262, 1670.9520303480226).expect("valid reference ratings");
    let cases = [
        (
            Odds {
                home: 81.0,
                draw: 82.0,
                away: 83.0,
            },
            Outcome::Home,
        ),
        (
            Odds {
                home: 83.0,
                draw: 81.0,
                away: 82.0,
            },
            Outcome::Draw,
        ),
        (
            Odds {
                home: 83.0,
                draw: 82.0,
                away: 81.0,
            },
            Outcome::Away,
        ),
        (
            Odds {
                home: 81.0,
                draw: 117.0,
                away: 112.0,
            },
            Outcome::Home,
        ),
    ];

    for (odds, expected) in cases {
        let prediction = predict_from_lambdas(expected_lambdas, odds).expect("valid prediction");
        let cheapest = [Outcome::Home, Outcome::Draw, Outcome::Away]
            .into_iter()
            .min_by(|a, b| odds.for_outcome(*a).total_cmp(&odds.for_outcome(*b)))
            .expect("three outcomes");

        assert_eq!(prediction.recommendation, expected);
        assert_eq!(prediction.recommendation, cheapest);
    }
}

#[test]
fn points_use_the_quotation_of_the_final_outcome() {
    let odds = Odds {
        home: 48.0,
        draw: 155.0,
        away: 30.0,
    };

    assert_close(
        points(Score { home: 2, away: 0 }, Score { home: 2, away: 0 }, odds),
        48.0,
        1e-12,
    );
    assert_close(
        points(Score { home: 2, away: 0 }, Score { home: 1, away: 0 }, odds),
        48.0,
        1e-12,
    );
    assert_close(
        points(Score { home: 1, away: 1 }, Score { home: 1, away: 5 }, odds),
        0.0,
        1e-12,
    );
}

#[test]
fn stronger_home_team_gets_higher_lambda() {
    let lambdas = lambdas(1800.0, 1500.0).expect("valid Elo ratings");

    assert_close(lambdas.home, 2.112909, 1e-6);
    assert_close(lambdas.away, 0.669091, 1e-6);
    assert!(lambdas.home > lambdas.away);
    assert!(lambdas.home > 0.0);
    assert!(lambdas.away > 0.0);
}

#[test]
fn the_lambda_entry_point_matches_the_elo_path() {
    let odds = Odds {
        home: 81.0,
        draw: 117.0,
        away: 112.0,
    };
    let home_elo = 1684.0983240112262;
    let away_elo = 1670.9520303480226;
    let expected = lambdas(home_elo, away_elo).expect("valid Elo ratings");

    assert_eq!(
        predict_from_lambdas(expected, odds).expect("valid lambdas"),
        predict(home_elo, away_elo, odds).expect("valid Elo ratings")
    );
}

#[test]
fn invalid_lambdas_are_rejected() {
    let odds = Odds {
        home: 81.0,
        draw: 117.0,
        away: 112.0,
    };
    for lambdas in [
        Lambdas {
            home: 0.0,
            away: 1.0,
        },
        Lambdas {
            home: 1.0,
            away: f64::NAN,
        },
    ] {
        assert!(matches!(
            predict_from_lambdas(lambdas, odds),
            Err(ModelError::InvalidLambdas)
        ));
    }
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
