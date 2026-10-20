//! Club-name normalization for Ligue 1 providers.
//!
//! MPP providers use varying spellings (`LOSC`, `OM`, `Paris SG`, …). Names
//! are folded with NFKD, stripped of combining marks, lowercased and mapped
//! to a canonical Ligue 1 name. A second alias table maps those canonical
//! names to the spellings used by ClubElo.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Canonical names of the 18 Ligue 1 clubs.
pub const CANONICAL_CLUBS: [&str; 18] = [
    "AJ Auxerre",
    "Angers",
    "Brest",
    "Le Havre",
    "Le Mans FC",
    "Lille OSC",
    "Lorient",
    "Lyon",
    "Marseille",
    "Monaco",
    "Nice",
    "PSG",
    "Paris FC",
    "RC Lens",
    "Rennes",
    "Strasbourg",
    "Toulouse",
    "Troyes AC",
];

/// Normalizes a club name for lookups.
///
/// Applies NFKD, removes combining marks, lowercases, converts hyphens to
/// spaces and collapses whitespace.
pub fn normalize_name(name: &str) -> String {
    let decomposed: String = name
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .collect();

    decomposed
        .to_lowercase()
        .replace('-', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Maps a provider club name to a canonical Ligue 1 name.
///
/// Returns [`None`] for clubs outside the canonical Ligue 1 list.
pub fn canonical_name(name: &str) -> Option<&'static str> {
    let normalized = normalize_name(name);
    let canonical = match normalized.as_str() {
        "aj auxerre" | "auxerre" => "AJ Auxerre",
        "angers" | "sco angers" | "angers sco" | "sco" => "Angers",
        "brest" | "stade brestois" | "stade brestois 29" => "Brest",
        "le havre" | "havre ac" | "le havre ac" | "hac" => "Le Havre",
        "le mans fc" | "le mans" => "Le Mans FC",
        "lille osc" | "losc" | "lille" | "losc lille" => "Lille OSC",
        "lorient" | "fc lorient" => "Lorient",
        "lyon" | "ol" | "olympique lyonnais" => "Lyon",
        "marseille" | "om" | "olympique de marseille" | "olympique marseille" => "Marseille",
        "monaco" | "as monaco" | "as monaco fc" => "Monaco",
        "nice" | "ogc nice" => "Nice",
        "psg" | "paris sg" | "paris saint germain" => "PSG",
        "paris fc" | "paris football club" => "Paris FC",
        "rc lens" | "lens" | "racing club de lens" => "RC Lens",
        "rennes" | "stade rennais" | "stade rennais fc" => "Rennes",
        "strasbourg" | "rc strasbourg" | "rc strasbourg alsace" => "Strasbourg",
        "toulouse" | "tfc" | "toulouse fc" => "Toulouse",
        "troyes ac" | "troyes" | "es troyes ac" | "estac" | "estac troyes" => "Troyes AC",
        _ => return None,
    };
    Some(canonical)
}

/// Maps a canonical club name to its ClubElo spelling.
///
/// Returns [`None`] when the name is not a canonical Ligue 1 club.
pub fn clubelo_name(canonical: &str) -> Option<&str> {
    let clubelo = match canonical {
        "AJ Auxerre" => "Auxerre",
        "Le Mans FC" => "Le Mans",
        "Lille OSC" => "Lille",
        "PSG" => "Paris SG",
        "RC Lens" => "Lens",
        "Troyes AC" => "Troyes",
        name => name,
    };

    CANONICAL_CLUBS.contains(&canonical).then_some(clubelo)
}
