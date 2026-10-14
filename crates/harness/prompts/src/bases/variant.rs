//! Which base instructions a recommended profile carries.
//!
//! The parity work replaced a ~500-character family base with a much longer
//! one, and the eval plane has to be able to measure the two against each
//! other on the same task set. The previous constants are kept whole in
//! [`super::legacy`] and selected by this switch; nothing else in the crate
//! branches on it.
//!
//! The variant is read from the process environment once per profile build, so
//! an eval arm sets it the same way it sets any other harness input. Anything
//! other than the two names below is a configuration mistake and is reported
//! as one rather than silently falling back.

/// The environment variable that selects the variant.
pub const PLAK_PROMPT_VARIANT_ENV: &str = "PLAK_PROMPT_VARIANT";

/// The name of the default variant.
pub const PLAK_PROMPT_VARIANT_PARITY: &str = "parity";

/// The name of the retired variant.
pub const PLAK_PROMPT_VARIANT_LEGACY: &str = "legacy";

/// The base-instruction variant a recommended profile is built with.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PromptVariant {
    /// The vendored Codex sections, and the Plakous-authored equivalents for
    /// the families whose own prompts are not open source. The default.
    #[default]
    Parity,
    /// The short family bases that shipped before the parity work: tool
    /// naming and the no-invented-tools guardrail, and nothing else.
    Legacy,
}

impl PromptVariant {
    /// The variant named by `name`, or `None` when the name is not one of the
    /// two.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            PLAK_PROMPT_VARIANT_PARITY => Some(Self::Parity),
            PLAK_PROMPT_VARIANT_LEGACY => Some(Self::Legacy),
            _ => None,
        }
    }

    /// The name this variant is selected by.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Parity => PLAK_PROMPT_VARIANT_PARITY,
            Self::Legacy => PLAK_PROMPT_VARIANT_LEGACY,
        }
    }

    /// The variant selected by [`PLAK_PROMPT_VARIANT_ENV`].
    ///
    /// An unset or empty variable selects [`PromptVariant::Parity`]. A value
    /// that names no variant also selects `Parity`, because a profile build is
    /// not a place to fail a run over a measurement knob; the name is
    /// validated where the eval plane sets it, and
    /// [`PromptVariant::from_name`] is the check.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var(PLAK_PROMPT_VARIANT_ENV) {
            Ok(value) if !value.is_empty() => Self::from_name(&value).unwrap_or_default(),
            Ok(_) | Err(_) => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_is_the_default_and_both_names_round_trip() {
        assert_eq!(PromptVariant::default(), PromptVariant::Parity);
        for variant in [PromptVariant::Parity, PromptVariant::Legacy] {
            assert_eq!(PromptVariant::from_name(variant.name()), Some(variant));
        }
        assert_eq!(PromptVariant::from_name("Parity"), None);
        assert_eq!(PromptVariant::from_name(""), None);
        assert_eq!(PromptVariant::from_name("baseline"), None);
    }

    #[test]
    fn the_environment_variable_is_named_for_the_crate() {
        assert_eq!(PLAK_PROMPT_VARIANT_ENV, "PLAK_PROMPT_VARIANT");
        assert_eq!(PLAK_PROMPT_VARIANT_PARITY, "parity");
        assert_eq!(PLAK_PROMPT_VARIANT_LEGACY, "legacy");
    }
}
