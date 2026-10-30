//! Season score report for the persisted ledger.

use std::collections::BTreeMap;
use std::fmt::Write;

use chrono_tz::Europe::Paris;

use crate::state::{ForecastOrigin, SeasonMatch, State};

/// Renders one line per match plus the season totals.
///
/// The MPP points are authoritative when present. A match without MPP points
/// falls back to the points computed from the stored forecast quotation and is
/// marked with `~`.
pub fn score_report(state: &State) -> String {
    let mut rows: Vec<&SeasonMatch> = state.season_matches.values().collect();
    if rows.is_empty() {
        return "No matches in the season ledger.\n".to_owned();
    }

    rows.sort_by(|left, right| {
        left.kickoff
            .cmp(&right.kickoff)
            .then_with(|| left.match_id.cmp(&right.match_id))
    });

    let mut output = String::new();
    let _ = writeln!(
        output,
        "{:<10} {:<30} {:>5} {:>9} {:>7} {:>8} {:>5} {:>9} {:>10}",
        "date", "fixture", "pick", "result", "points", "computed", "modal", "modal_pts", "source"
    );

    let mut totals: BTreeMap<u16, SeasonTotals> = BTreeMap::new();

    for row in &rows {
        let date = row.kickoff.with_timezone(&Paris).format("%Y-%m-%d");
        let fixture = format!("{} - {}", row.home, row.away);
        let pick = row
            .pick
            .map(|score| score.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let result = match (row.result, row.status.as_deref()) {
            (Some(score), _) => score.to_string(),
            (None, Some(status)) => status.to_owned(),
            (None, None) => "pending".to_owned(),
        };

        let season = totals.entry(row.season).or_default();
        let (points, estimated) = match (&row.points, row.computed_points) {
            (Some(points), _) if points.total.is_some() => (points.total, false),
            (_, Some(computed)) => (Some(computed.round() as i64), true),
            _ => (None, false),
        };
        let points_text = match points {
            Some(points) if estimated => format!("~{points}"),
            Some(points) => points.to_string(),
            None => "-".to_owned(),
        };

        if let Some(points) = points {
            season.points += points;
            if estimated {
                season.estimated += 1;
            }
        }
        if row.result.is_some() {
            season.played += 1;
        }
        if let Some(computed) = row.computed_points {
            season.computed += computed;
        }
        if let Some(modal_points) = row.modal_points {
            season.modal_points += modal_points;
            season.modal_matches += 1;
        }

        let computed = row
            .computed_points
            .map(|points| format!("{points:.0}"))
            .unwrap_or_else(|| "-".to_owned());
        let modal = row
            .modal
            .map(|score| score.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let modal_points = row
            .modal_points
            .map(|points| format!("{points:.0}"))
            .unwrap_or_else(|| "-".to_owned());
        let source = source_label(row.source);

        let _ = writeln!(
            output,
            "{date:<10} {fixture:<30} {pick:>5} {result:>9} {points_text:>7} {computed:>8} {modal:>5} {modal_points:>9} {source:>10}"
        );
    }

    output.push('\n');
    for (season, totals) in totals {
        let estimated = if totals.estimated > 0 {
            format!("; estimated for {} match(es)", totals.estimated)
        } else {
            String::new()
        };
        let _ = writeln!(
            output,
            "season {}/{:02}: {} points from {} match(es); computed {:.0}; modal {:.0} from {} match(es){}",
            season,
            (season + 1) % 100,
            totals.points,
            totals.played,
            totals.computed,
            totals.modal_points,
            totals.modal_matches,
            estimated
        );
    }

    let pending = rows.iter().filter(|row| row.result.is_none()).count();
    if pending > 0 {
        let _ = writeln!(output, "pending: {} match(es) without a result", pending);
    }

    output
}

fn source_label(source: Option<ForecastOrigin>) -> &'static str {
    match source {
        Some(ForecastOrigin::Market) => "market",
        Some(ForecastOrigin::Elo) => "elo",
        Some(ForecastOrigin::EloCached) => "elo_cached",
        None => "-",
    }
}

#[derive(Default)]
struct SeasonTotals {
    points: i64,
    computed: f64,
    modal_points: f64,
    modal_matches: usize,
    played: usize,
    estimated: usize,
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::model::{Odds, Score};
    use crate::state::{Mode, PointsRecord};

    fn at(day: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, 18, 45, 0)
            .single()
            .expect("valid timestamp")
    }

    fn played_match() -> SeasonMatch {
        SeasonMatch {
            match_id: "match-1".to_owned(),
            kickoff: at(26),
            season: 2026,
            home: "Paris SG".to_owned(),
            away: "OL".to_owned(),
            pick: Some(Score { home: 2, away: 0 }),
            result: Some(Score { home: 2, away: 0 }),
            points: Some(PointsRecord {
                base: Some(48),
                exact: Some(30),
                extra: Some(0),
                bonus: Some(0),
                total: Some(78),
                rarity_level: None,
            }),
            computed_points: Some(78.0),
            modal_points: Some(48.0),
            source: Some(ForecastOrigin::Market),
            odds: Some(Odds {
                home: 48.0,
                draw: 155.0,
                away: 30.0,
            }),
            modal: Some(Score { home: 2, away: 1 }),
            status: Some("decided".to_owned()),
            observed_at: at(30),
        }
    }

    fn pending_match() -> SeasonMatch {
        SeasonMatch {
            match_id: "match-2".to_owned(),
            kickoff: at(27),
            season: 2026,
            home: "Le Havre".to_owned(),
            away: "Paris SG".to_owned(),
            pick: Some(Score { home: 1, away: 1 }),
            result: None,
            points: None,
            computed_points: None,
            modal_points: None,
            source: Some(ForecastOrigin::Market),
            odds: Some(Odds {
                home: 155.0,
                draw: 155.0,
                away: 30.0,
            }),
            modal: Some(Score { home: 0, away: 2 }),
            status: Some("postponed".to_owned()),
            observed_at: at(30),
        }
    }

    #[test]
    fn report_prints_matches_totals_and_pending_rows() {
        let mut state = State::new(Mode::Write);
        state
            .season_matches
            .insert("match-1".to_owned(), played_match());
        state
            .season_matches
            .insert("match-2".to_owned(), pending_match());

        let report = score_report(&state);

        assert!(report.contains("2026-10-26"));
        assert!(report.contains("Paris SG - OL"));
        assert!(report.contains("market"));
        assert!(report.contains("season 2026/27: 78 points from 1 match(es)"));
        assert!(report.contains("modal 48 from 1 match(es)"));
        assert!(report.contains("pending: 1 match(es) without a result"));
        assert!(report.contains("postponed"));
    }

    #[test]
    fn report_marks_computed_points_when_mpp_points_are_missing() {
        let mut row = played_match();
        row.points.as_mut().expect("points").total = None;

        let mut state = State::new(Mode::Write);
        state.season_matches.insert("match-1".to_owned(), row);

        let report = score_report(&state);

        assert!(report.contains("~78"));
        assert!(report.contains("estimated for 1 match(es)"));
    }

    #[test]
    fn empty_ledger_has_a_clear_message() {
        assert_eq!(
            score_report(&State::new(Mode::Dry)),
            "No matches in the season ledger.\n"
        );
    }
}
