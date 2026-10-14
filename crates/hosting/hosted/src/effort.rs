//! The one named reasoning-effort ladder the product surfaces share.
//!
//! The `--effort` CLI flag, a template's stored effort, and a persisted
//! session effort name the same levels. There is no `/effort` command and no
//! Models-tab effort bar: a session's effort changes only through a profile.
//! Before this table each surface carried its own literal list. Every reader
//! uses this table instead so a level cannot exist on one and be missing from
//! another.

/// Named effort levels, weakest first. A token budget is not a *named* level
/// and is not listed here; it is expressed through the model binding.
pub const LEVELS: [&str; 7] = ["none", "low", "medium", "high", "xhigh", "max", "ultra"];

/// The levels joined for a usage message (`low|medium|…`).
#[must_use]
pub fn levels_pipe_list() -> String {
    LEVELS.join("|")
}

/// The canonical spelling of `value` when it names a level, else `None`.
///
/// Matching is ASCII case-insensitive; callers keep the returned `'static`
/// spelling rather than the operator's casing.
#[must_use]
pub fn parse(value: &str) -> Option<&'static str> {
    let value = value.trim();
    LEVELS
        .into_iter()
        .find(|level| level.eq_ignore_ascii_case(value))
}

#[cfg(test)]
mod tests {
    use super::{LEVELS, levels_pipe_list, parse};

    #[test]
    fn every_level_round_trips_and_case_is_folded() {
        for level in LEVELS {
            assert_eq!(parse(level), Some(level));
            assert_eq!(parse(&level.to_ascii_uppercase()), Some(level));
        }
        assert_eq!(parse("  high "), Some("high"));
        assert_eq!(parse("ultra"), Some("ultra"), "ultra is admitted");
        assert_eq!(parse("turbo"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn the_usage_list_prints_every_level_in_ladder_order() {
        assert_eq!(levels_pipe_list(), "none|low|medium|high|xhigh|max|ultra");
    }
}
