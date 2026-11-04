//! The guard catalogue must name places that still exist.
//!
//! A guard is a decision the service makes about spending money, serving stale data or
//! refusing a request. When one moves, this test fails until the catalogue moves with it; a
//! catalogue that silently describes the past is worse than none, because it is trusted.

use std::path::Path;

/// A relative source path and a literal snippet that must still appear in it.
struct Location {
    path: String,
    snippet: String,
}

#[test]
fn every_catalogue_entry_names_a_live_guard() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let catalogue = std::fs::read_to_string(root.join("docs").join("GUARD_CATALOGUE.md"))
        .expect("docs/GUARD_CATALOGUE.md is missing");

    let locations: Vec<Location> = catalogue.lines().filter_map(parse_location).collect();
    assert!(
        locations.len() >= 10,
        "only {} guard locations parsed; the catalogue format or content has broken",
        locations.len()
    );

    for location in &locations {
        assert!(
            location.path.starts_with("src/") || location.path.starts_with("tests/"),
            "{} is not a source path",
            location.path
        );
        let source = std::fs::read_to_string(root.join(&location.path))
            .unwrap_or_else(|error| panic!("{} cannot be read: {error}", location.path));
        assert!(
            source.contains(&location.snippet),
            "{} no longer contains {:?}; the catalogue is stale",
            location.path,
            location.snippet
        );
    }
}

/// `- **Location:** `path` · `snippet``, or `None` for any other line.
fn parse_location(line: &str) -> Option<Location> {
    let rest = line.trim().strip_prefix("- **Location:** ")?;
    let (path, snippet) = rest.split_once(" · ")?;
    Some(Location {
        path: path.trim().trim_matches('`').to_string(),
        snippet: snippet.trim().trim_matches('`').to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_location_line_is_taken_apart_into_a_path_and_a_snippet() {
        let location =
            parse_location("- **Location:** `src/products.rs` · `fn use_bulk(`").unwrap();
        assert_eq!(location.path, "src/products.rs");
        assert_eq!(location.snippet, "fn use_bulk(");
    }

    #[test]
    fn prose_lines_are_not_locations() {
        assert!(parse_location("## G-01 · Exchange resolution").is_none());
        assert!(parse_location("- **Config:** none").is_none());
    }
}
