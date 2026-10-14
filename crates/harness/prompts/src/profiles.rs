//! Recommended harness profiles, one per coding family.
//!
//! A profile is data: an id, a base instruction, and the strategies that say
//! how a turn is composed. They live beside the text they name, so a profile
//! and its base instruction are reviewed and pinned together.
//!
//! Each family has two constructors. `recommended()` and `code_mode()` carry
//! the default base-instruction variant and read nothing; the `_for` forms
//! take a [`crate::bases::PromptVariant`]. A product that offers the
//! `PLAK_PROMPT_VARIANT` switch calls the `_for` form with
//! `PromptVariant::from_env()`, so the environment is read once, above the
//! library, and spec 01 §2.1 still holds: nothing under construction decides
//! model-visible text from the process environment.

pub use plakous_agent::recommended_neutral_coding_profile as neutral_coding;

/// Codex OAuth coding family recommended harness profiles.
pub mod codex {
    use plakous_agent::{
        CachePreferenceStrategy, ContinuationPreferenceStrategy, HarnessProfileId,
        HarnessProfileProvenance, HarnessProfileSpec, HarnessProfileSpecBuilder,
        InstructionCompositionStrategy, ProfileError, ProfileSemanticVersion, ReasoningEffort,
        ReasoningPreferenceStrategy, ToolExposureStrategy,
    };

    use crate::bases::PromptVariant;

    /// Returns the data-only Codex coding profile shipped by the facade.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the reviewed constants fail validation.
    pub fn recommended() -> Result<HarnessProfileSpec, ProfileError> {
        recommended_for(PromptVariant::default())
    }

    /// Returns the Codex coding profile carrying `variant`'s base instruction.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the reviewed constants fail validation.
    pub fn recommended_for(variant: PromptVariant) -> Result<HarnessProfileSpec, ProfileError> {
        HarnessProfileSpecBuilder::new()
            .id(HarnessProfileId::new("plakous.codex.coding.v1")?)
            .semantic_version(ProfileSemanticVersion::new(0, 2, 0))
            .provenance(HarnessProfileProvenance::Recommended)
            .display_name("Codex coding")
            .base_instructions(crate::bases::codex_direct_base_instructions_for(variant))
            .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
            .tool_exposure_strategy(ToolExposureStrategy::None)
            .reasoning_strategy(ReasoningPreferenceStrategy::PreferEffort)
            .preferred_reasoning_effort(Some(ReasoningEffort::High))
            .continuation_strategy(ContinuationPreferenceStrategy::PreferNative)
            .cache_strategy(CachePreferenceStrategy::Disabled)
            .build()
    }

    /// Returns the exact Codex native local Code Mode coding profile.
    ///
    /// The provider-visible surface is only `exec`/`wait`; canonical coding
    /// capabilities are nested JavaScript functions and must never be
    /// described as direct model tools. `nested_tool_names` is the compiled
    /// nested catalog (core plus extras) rendered into the base instruction.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the reviewed constants fail validation
    /// or `nested_tool_names` is empty.
    pub fn code_mode(nested_tool_names: &[&str]) -> Result<HarnessProfileSpec, ProfileError> {
        code_mode_for(PromptVariant::default(), nested_tool_names)
    }

    /// Returns the Code Mode profile carrying `variant`'s base instruction.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the reviewed constants fail validation
    /// or `nested_tool_names` is empty.
    pub fn code_mode_for(
        variant: PromptVariant,
        nested_tool_names: &[&str],
    ) -> Result<HarnessProfileSpec, ProfileError> {
        if nested_tool_names.is_empty() {
            return Err(ProfileError::InvalidIdentity);
        }
        HarnessProfileSpecBuilder::new()
            .id(HarnessProfileId::new("plakous.codex.code-mode.v1")?)
            .semantic_version(ProfileSemanticVersion::new(0, 2, 0))
            .provenance(HarnessProfileProvenance::Recommended)
            .display_name("Codex Code Mode coding")
            .base_instructions(crate::bases::render_codex_code_mode_base_instructions_for(
                variant,
                nested_tool_names,
            ))
            .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
            .tool_exposure_strategy(ToolExposureStrategy::None)
            .reasoning_strategy(ReasoningPreferenceStrategy::PreferEffort)
            .preferred_reasoning_effort(Some(ReasoningEffort::High))
            .continuation_strategy(ContinuationPreferenceStrategy::PreferNative)
            .cache_strategy(CachePreferenceStrategy::Disabled)
            .build()
    }

    #[cfg(test)]
    #[allow(
        clippy::expect_used,
        reason = "unit tests may fail fast on invalid reviewed profile constants"
    )]
    mod tests {
        use super::*;

        #[test]
        fn recommended_profile_is_high_effort_and_catalog_bound() {
            let profile = recommended().expect("reviewed Codex profile");
            assert_eq!(profile.id().as_str(), "plakous.codex.coding.v1");
            assert_eq!(profile.provenance(), HarnessProfileProvenance::Recommended);
            assert_eq!(profile.tool_exposure_strategy(), ToolExposureStrategy::None);
            assert_eq!(
                profile.preferred_reasoning_effort(),
                Some(ReasoningEffort::High)
            );
        }

        #[test]
        fn code_mode_profile_names_only_outer_tools_as_direct() {
            let nested = ["exec_command", "write_stdin", "apply_patch", "update_plan"];
            let profile = code_mode(&nested).expect("reviewed Code Mode profile");
            assert_eq!(profile.id().as_str(), "plakous.codex.code-mode.v1");
            assert_eq!(
                profile.semantic_version(),
                ProfileSemanticVersion::new(0, 2, 0)
            );
            assert!(profile.base_instructions().contains("exec and wait"));
            assert!(profile.base_instructions().contains("tools object"));
            assert!(profile.base_instructions().contains(
                "nested tools on its tools object: exec_command, write_stdin, apply_patch, update_plan."
            ));
            assert!(!profile.base_instructions().contains("read_file"));
            assert!(!profile.base_instructions().contains("inspect_image"));
            assert!(!profile.base_instructions().contains("full set"));
        }

        #[test]
        fn code_mode_profile_lists_caller_supplied_nested_names() {
            let nested = [
                "exec_command",
                "write_stdin",
                "apply_patch",
                "update_plan",
                "spawn_agent",
            ];
            let profile = code_mode(&nested).expect("extended Code Mode profile");
            assert!(profile.base_instructions().contains("spawn_agent"));
            assert!(!profile.base_instructions().contains("{names}"));
        }
    }
}

/// SuperGrok / Grok coding family recommended harness profiles.
pub mod grok {
    use plakous_agent::{
        CachePreferenceStrategy, ContinuationPreferenceStrategy, HarnessProfileId,
        HarnessProfileProvenance, HarnessProfileSpec, HarnessProfileSpecBuilder,
        InstructionCompositionStrategy, ProfileError, ProfileSemanticVersion, ReasoningEffort,
        ReasoningPreferenceStrategy, ToolExposureStrategy,
    };

    use crate::bases::PromptVariant;

    /// Returns the data-only Grok coding profile shipped by the facade.
    ///
    /// Provider-specific recommended data belongs at this composition
    /// boundary, outside the provider-neutral agent and model crates.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the reviewed constants fail validation.
    pub fn recommended() -> Result<HarnessProfileSpec, ProfileError> {
        recommended_for(PromptVariant::default())
    }

    /// Returns the profile carrying `variant`'s base instruction.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the reviewed constants fail validation.
    pub fn recommended_for(variant: PromptVariant) -> Result<HarnessProfileSpec, ProfileError> {
        HarnessProfileSpecBuilder::new()
            .id(HarnessProfileId::new("plakous.grok.coding.v1")?)
            .semantic_version(ProfileSemanticVersion::new(0, 4, 0))
            .provenance(HarnessProfileProvenance::Recommended)
            .display_name("Grok coding")
            .base_instructions(crate::bases::grok_direct_base_instructions_for(variant))
            .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
            .tool_exposure_strategy(ToolExposureStrategy::None)
            .reasoning_strategy(ReasoningPreferenceStrategy::PreferEffort)
            .preferred_reasoning_effort(Some(ReasoningEffort::Low))
            .continuation_strategy(ContinuationPreferenceStrategy::PreferNative)
            .cache_strategy(CachePreferenceStrategy::Disabled)
            .build()
    }

    #[cfg(test)]
    #[allow(
        clippy::expect_used,
        reason = "unit tests may fail fast on invalid reviewed profile constants"
    )]
    mod tests {
        use super::*;

        #[test]
        fn recommended_profile_is_low_effort_and_provider_scoped() {
            let profile = recommended().expect("reviewed Grok profile");
            assert_eq!(profile.id().as_str(), "plakous.grok.coding.v1");
            assert_eq!(profile.provenance(), HarnessProfileProvenance::Recommended);
            assert_eq!(
                profile.reasoning_strategy(),
                ReasoningPreferenceStrategy::PreferEffort
            );
            assert_eq!(
                profile.preferred_reasoning_effort(),
                Some(ReasoningEffort::Low)
            );
            assert_eq!(profile.cache_strategy(), CachePreferenceStrategy::Disabled);
            assert_eq!(profile.tool_exposure_strategy(), ToolExposureStrategy::None);
            let neutral = super::super::neutral_coding().expect("neutral profile");
            assert_ne!(profile.id(), neutral.id());
            assert_ne!(profile.base_instructions(), neutral.base_instructions());
        }
    }
}

/// Claude OAuth coding family recommended harness profiles.
pub mod claude {
    use plakous_agent::{
        CachePreferenceStrategy, ContinuationPreferenceStrategy, HarnessProfileId,
        HarnessProfileProvenance, HarnessProfileSpec, HarnessProfileSpecBuilder,
        InstructionCompositionStrategy, ProfileError, ProfileSemanticVersion, ReasoningEffort,
        ReasoningPreferenceStrategy, ToolExposureStrategy,
    };

    use crate::bases::PromptVariant;

    /// Returns the data-only Claude OAuth coding profile shipped by the facade.
    ///
    /// The gateway identity line is prepended by the Messages dialect, not here.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the reviewed constants fail validation.
    pub fn recommended() -> Result<HarnessProfileSpec, ProfileError> {
        recommended_for(PromptVariant::default())
    }

    /// Returns the profile carrying `variant`'s base instruction.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the reviewed constants fail validation.
    pub fn recommended_for(variant: PromptVariant) -> Result<HarnessProfileSpec, ProfileError> {
        HarnessProfileSpecBuilder::new()
            .id(HarnessProfileId::new("plakous.claude.oauth.coding.v1")?)
            .semantic_version(ProfileSemanticVersion::new(0, 2, 0))
            .provenance(HarnessProfileProvenance::Recommended)
            .display_name("Claude OAuth coding")
            .base_instructions(crate::bases::claude_direct_base_instructions_for(variant))
            .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
            .tool_exposure_strategy(ToolExposureStrategy::None)
            .reasoning_strategy(ReasoningPreferenceStrategy::PreferEffort)
            .preferred_reasoning_effort(Some(ReasoningEffort::Low))
            .continuation_strategy(ContinuationPreferenceStrategy::PreferNative)
            .cache_strategy(CachePreferenceStrategy::PreferStablePrefix)
            .build()
    }

    #[cfg(test)]
    #[allow(
        clippy::expect_used,
        reason = "unit tests may fail fast on invalid reviewed profile constants"
    )]
    mod tests {
        use super::*;

        #[test]
        fn recommended_profile_prefers_cache_and_six_tools() {
            let profile = recommended().expect("reviewed Claude OAuth profile");
            assert_eq!(profile.id().as_str(), "plakous.claude.oauth.coding.v1");
            assert_eq!(
                profile.cache_strategy(),
                CachePreferenceStrategy::PreferStablePrefix
            );
            assert_eq!(profile.tool_exposure_strategy(), ToolExposureStrategy::None);
            assert_eq!(
                profile.preferred_reasoning_effort(),
                Some(ReasoningEffort::Low)
            );
            assert!(
                !profile
                    .base_instructions()
                    .contains("You are Claude Code, Anthropic's official CLI for Claude.")
            );
        }
    }
}
