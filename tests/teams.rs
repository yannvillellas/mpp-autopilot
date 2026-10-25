//! Club-name normalization and provider aliases.

use mpp_autopilot::teams::{CANONICAL_CLUBS, canonical_name, normalize_name};

#[test]
fn mpp_variants_map_to_canonical_names() {
    let cases = [
        ("LOSC", "Lille OSC"),
        ("OM", "Marseille"),
        ("OL", "Lyon"),
        ("AS Monaco", "Monaco"),
        ("Havre AC", "Le Havre"),
        ("Paris SG", "PSG"),
        ("PSG", "PSG"),
        ("Le Mans FC", "Le Mans FC"),
        ("Troyes AC", "Troyes AC"),
        ("RC Lens", "RC Lens"),
        ("Lens", "RC Lens"),
        ("AJ Auxerre", "AJ Auxerre"),
        ("Auxerre", "AJ Auxerre"),
        ("Paris FC", "Paris FC"),
        ("Angers SCO", "Angers"),
        ("SCO", "Angers"),
        ("LOSC Lille", "Lille OSC"),
        ("ESTAC Troyes", "Troyes AC"),
        ("HAC", "Le Havre"),
    ];

    for (input, expected) in cases {
        assert_eq!(canonical_name(input), Some(expected), "input: {input}");
    }
}

#[test]
fn mpp_club_names_map_to_canonical_names() {
    let cases = [
        ("AS Monaco", "Monaco"),
        ("Angers SCO", "Angers"),
        ("AJ Auxerre", "AJ Auxerre"),
        ("Brest", "Brest"),
        ("Havre AC", "Le Havre"),
        ("Le Mans FC", "Le Mans FC"),
        ("LOSC", "Lille OSC"),
        ("Lorient", "Lorient"),
        ("OL", "Lyon"),
        ("OM", "Marseille"),
        ("Nice", "Nice"),
        ("Paris FC", "Paris FC"),
        ("Paris SG", "PSG"),
        ("RC Lens", "RC Lens"),
        ("Rennes", "Rennes"),
        ("Strasbourg", "Strasbourg"),
        ("Toulouse", "Toulouse"),
        ("ESTAC Troyes", "Troyes AC"),
    ];

    for (input, expected) in cases {
        assert_eq!(canonical_name(input), Some(expected), "input: {input}");
    }
}

#[test]
fn the_odds_api_spellings_map_to_canonical_names() {
    let cases = [
        ("AS Monaco", "Monaco"),
        ("Angers", "Angers"),
        ("Auxerre", "AJ Auxerre"),
        ("Brest", "Brest"),
        ("Le Havre", "Le Havre"),
        ("Le Mans FC", "Le Mans FC"),
        ("Lille", "Lille OSC"),
        ("Lorient", "Lorient"),
        ("Lyon", "Lyon"),
        ("Marseille", "Marseille"),
        ("Nice", "Nice"),
        ("Paris FC", "Paris FC"),
        ("Paris Saint Germain", "PSG"),
        ("RC Lens", "RC Lens"),
        ("Rennes", "Rennes"),
        ("Strasbourg", "Strasbourg"),
        ("Toulouse", "Toulouse"),
        ("Troyes", "Troyes AC"),
    ];

    for (input, expected) in cases {
        assert_eq!(canonical_name(input), Some(expected), "input: {input}");
    }
}

#[test]
fn football_data_spellings_map_to_canonical_names() {
    let cases = [
        ("Ajaccio", "AC Ajaccio"),
        ("Bordeaux", "Bordeaux"),
        ("Clermont", "Clermont Foot"),
        ("Dijon", "Dijon FCO"),
        ("Metz", "FC Metz"),
        ("Nantes", "FC Nantes"),
        ("Montpellier", "Montpellier"),
        ("Nimes", "Nimes"),
        ("Reims", "Reims"),
        ("St Etienne", "Saint-Etienne"),
    ];

    for (input, expected) in cases {
        assert_eq!(canonical_name(input), Some(expected), "input: {input}");
    }
}

#[test]
fn aliases_compose_with_normalization() {
    assert_eq!(canonical_name("Paris Saint-Germain"), Some("PSG"));
    assert_eq!(canonical_name("Le Havre AC"), Some("Le Havre"));
    assert_eq!(canonical_name("Le Mans"), Some("Le Mans FC"));
    assert_eq!(canonical_name("Saint-Etienne"), Some("Saint-Etienne"));
}

#[test]
fn every_canonical_name_resolves_to_itself() {
    for club in CANONICAL_CLUBS {
        assert_eq!(canonical_name(club), Some(club));
    }
}

#[test]
fn unknown_clubs_are_rejected() {
    assert_eq!(canonical_name("Grenoble Foot 38"), None);
}

#[test]
fn normalization_folds_accents_case_hyphens_and_spaces() {
    assert_eq!(normalize_name("  Évian-Thonon  "), "evian thonon");
    assert_eq!(normalize_name("Paris Saint-Germain"), "paris saint germain");
    assert_eq!(normalize_name("Le   Havre"), "le havre");
    assert_eq!(normalize_name("AJ Auxerre"), "aj auxerre");
    assert_eq!(normalize_name("Saint-Étienne"), "saint etienne");
}
