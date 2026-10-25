//! Club-name normalization for Ligue 1 providers.
//!
//! Providers use varying spellings (`LOSC`, `OM`, `Paris SG`, `St Etienne`, …).
//! Names are folded with NFKD, stripped of combining marks, lowercased and
//! mapped to a canonical club name. Football-Data results use the same alias
//! table.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Canonical names of the Ligue 1 clubs since 2020/21.
///
/// Relegated clubs stay in the list so their rating is kept for a possible
/// return to the top flight.
pub const CANONICAL_CLUBS: [&str; 28] = [
    "AC Ajaccio",
    "AJ Auxerre",
    "Angers",
    "Bordeaux",
    "Brest",
    "Clermont Foot",
    "Dijon FCO",
    "FC Metz",
    "FC Nantes",
    "Le Havre",
    "Le Mans FC",
    "Lille OSC",
    "Lorient",
    "Lyon",
    "Marseille",
    "Monaco",
    "Montpellier",
    "Nice",
    "Nimes",
    "Paris FC",
    "PSG",
    "RC Lens",
    "Reims",
    "Rennes",
    "Saint-Etienne",
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
        "ac ajaccio" | "ajaccio" => "AC Ajaccio",
        "aj auxerre" | "auxerre" => "AJ Auxerre",
        "angers" | "sco angers" | "angers sco" | "sco" => "Angers",
        "bordeaux" | "girondins de bordeaux" | "fc girondins de bordeaux" => "Bordeaux",
        "brest" | "stade brestois" | "stade brestois 29" => "Brest",
        "clermont" | "clermont foot" | "clermont foot 63" => "Clermont Foot",
        "dijon" | "dijon fco" => "Dijon FCO",
        "metz" | "fc metz" => "FC Metz",
        "nantes" | "fc nantes" => "FC Nantes",
        "le havre" | "havre ac" | "le havre ac" | "hac" => "Le Havre",
        "le mans fc" | "le mans" => "Le Mans FC",
        "lille osc" | "losc" | "lille" | "losc lille" => "Lille OSC",
        "lorient" | "fc lorient" => "Lorient",
        "lyon" | "ol" | "olympique lyonnais" => "Lyon",
        "marseille" | "om" | "olympique de marseille" | "olympique marseille" => "Marseille",
        "monaco" | "as monaco" | "as monaco fc" => "Monaco",
        "montpellier" | "montpellier hsc" | "mhsc" => "Montpellier",
        "nice" | "ogc nice" => "Nice",
        "nimes" | "nimes olympique" => "Nimes",
        "psg" | "paris sg" | "paris saint germain" => "PSG",
        "paris fc" | "paris football club" => "Paris FC",
        "rc lens" | "lens" | "racing club de lens" => "RC Lens",
        "reims" | "stade de reims" => "Reims",
        "rennes" | "stade rennais" | "stade rennais fc" => "Rennes",
        "st etienne" | "saint etienne" | "as saint etienne" | "asse" => "Saint-Etienne",
        "strasbourg" | "rc strasbourg" | "rc strasbourg alsace" => "Strasbourg",
        "toulouse" | "tfc" | "toulouse fc" => "Toulouse",
        "troyes ac" | "troyes" | "es troyes ac" | "estac" | "estac troyes" => "Troyes AC",
        _ => return None,
    };
    Some(canonical)
}
