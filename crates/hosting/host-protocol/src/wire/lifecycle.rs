//! Closed directory lifecycle refusals, independent of runtime-history archival.

use serde::{Deserialize, Serialize};

/// The most child/process/dependent ids in one lifecycle refusal.
pub const MAX_LIFECYCLE_IDS: usize = 100;

/// Invalid operator-title input; titles are never silently trimmed or truncated.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvalidTitleKind {
    /// Empty or whitespace-only.
    Empty,
    /// More than 128 Unicode scalars or 512 UTF-8 bytes.
    TooLong,
    /// Contains a Unicode control character.
    ControlCharacter,
}

/// Why a directory lifecycle mutation was refused.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleReason {
    /// No product record has that full id.
    NotFound,
    /// The requested override is not a valid title.
    InvalidTitle {
        /// Closed validation verdict.
        kind: InvalidTitleKind,
    },
    /// Archive/delete cannot prove quiescence.
    Busy {
        /// Root turn is busy.
        turn: bool,
        /// Bounded live child ids.
        #[schemars(length(max = 100))]
        children: Vec<String>,
        /// Additional child ids were omitted.
        children_truncated: bool,
        /// Bounded running process/job ids.
        #[schemars(length(max = 100))]
        processes: Vec<String>,
        /// Additional process ids were omitted.
        processes_truncated: bool,
        /// Some running state could not be proved idle.
        state_unknown: bool,
    },
    /// Runtime root or finished-child lease remains live after host release.
    Open,
    /// A child is live or its runtime head is not quiescent.
    LiveTree {
        /// Bounded child ids.
        #[schemars(length(max = 100))]
        children: Vec<String>,
        /// Additional ids were omitted.
        children_truncated: bool,
    },
    /// This tree requires explicit abandonment.
    RecoveryRequiredTree {
        /// Bounded child ids.
        #[schemars(length(max = 100))]
        children: Vec<String>,
        /// Additional ids were omitted.
        children_truncated: bool,
    },
    /// Other sessions retain this history; never cascade-delete.
    ForkDependency {
        /// Bounded dependent session ids.
        #[schemars(length(max = 100))]
        dependents: Vec<String>,
        /// Additional ids were omitted.
        dependents_truncated: bool,
    },
}

/// Validate a non-null title without modifying its bytes.
///
/// # Errors
/// Returns the exact closed validation reason; null clears the override upstream.
pub fn validate_title(title: &str) -> Result<(), InvalidTitleKind> {
    if title.chars().any(char::is_control) {
        return Err(InvalidTitleKind::ControlCharacter);
    }
    if title.len() > 512 || title.chars().count() > 128 {
        return Err(InvalidTitleKind::TooLong);
    }
    if title.chars().all(char::is_whitespace) {
        return Err(InvalidTitleKind::Empty);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_validation_preserves_unicode_and_refuses_lossy_repairs() {
        for title in ["", " \u{2003}"] {
            assert_eq!(validate_title(title), Err(InvalidTitleKind::Empty));
        }
        for title in ["a\nb", "a\0b", "\u{85}"] {
            assert_eq!(
                validate_title(title),
                Err(InvalidTitleKind::ControlCharacter)
            );
        }
        assert_eq!(validate_title(&"🦀".repeat(128)), Ok(()));
        assert_eq!(
            validate_title(&"a".repeat(129)),
            Err(InvalidTitleKind::TooLong)
        );
        assert_eq!(
            validate_title(&"🦀".repeat(129)),
            Err(InvalidTitleKind::TooLong)
        );
        assert_eq!(validate_title("  keep surrounding spaces  "), Ok(()));
        // Count scalars, not graphemes: a combining mark still consumes the bound.
        assert_eq!(
            validate_title(&"e\u{301}".repeat(65)),
            Err(InvalidTitleKind::TooLong)
        );
    }
}
