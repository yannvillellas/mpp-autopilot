//! Club-name normalization and provider aliases.

use mpp::teams::{CANONICAL_CLUBS, canonical_name, clubelo_name, normalize_name};

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
fn aliases_compose_with_normalization() {
    assert_eq!(canonical_name("Paris Saint-Germain"), Some("PSG"));
    assert_eq!(canonical_name("Le Havre AC"), Some("Le Havre"));
    assert_eq!(canonical_name("Le Mans"), Some("Le Mans FC"));
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
}

#[test]
fn clubelo_names_follow_the_alias_table() {
    assert_eq!(clubelo_name("AJ Auxerre"), Some("Auxerre"));
    assert_eq!(clubelo_name("Le Mans FC"), Some("Le Mans"));
    assert_eq!(clubelo_name("Lille OSC"), Some("Lille"));
    assert_eq!(clubelo_name("PSG"), Some("Paris SG"));
    assert_eq!(clubelo_name("RC Lens"), Some("Lens"));
    assert_eq!(clubelo_name("Troyes AC"), Some("Troyes"));
    assert_eq!(clubelo_name("Angers"), Some("Angers"));
    assert_eq!(clubelo_name("Grenoble"), None);
}

#[test]
fn every_canonical_club_has_a_clubelo_name() {
    for club in CANONICAL_CLUBS {
        assert!(clubelo_name(club).is_some(), "club: {club}");
    }
}
