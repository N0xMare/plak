//! One walker over every prompt pin this crate owns.
//!
//! This replaces the sha256 helper the prompt modules borrowed from
//! `occupancy_vcs` — a VCS digest doing prompt duty — and the per-constant
//! `(len, digest)` tuples that repeated the same assertion in five files. The
//! pin tables live with their text; this is the only thing that hashes.

/// Every instruction pin table in this crate, in module order.
///
/// The topology tool descriptions and error strings are not here: they travel
/// with the topology pack, and the crate that owns them
/// walks its own tables the same way.
#[cfg(test)]
#[must_use]
pub fn instruction_prompt_pins() -> Vec<&'static [plakous_tools::PromptPin]> {
    vec![
        crate::bases::codex_vendor::CODEX_VENDOR_PROMPT_PINS,
        crate::identity::IDENTITY_PROMPT_PINS,
        crate::ptc::PTC_PROMPT_PINS,
        crate::content_filter::CONTENT_FILTER_PROMPT_PINS,
    ]
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests {
    use std::collections::BTreeSet;

    use plakous_tools::{PromptDigest, drifted_pins};

    use super::instruction_prompt_pins;

    #[test]
    fn every_product_prompt_pin_matches_its_text() {
        let mut drifted = Vec::new();
        for table in instruction_prompt_pins() {
            for (id, digest) in drifted_pins(table) {
                drifted.push(format!("{id} -> \"{digest}\""));
            }
        }
        assert!(drifted.is_empty(), "re-pin:\n{}", drifted.join("\n"));
    }

    #[test]
    fn product_prompt_pin_ids_are_unique_and_no_string_is_pinned_twice() {
        let mut ids = BTreeSet::new();
        let mut digests = BTreeSet::new();
        for table in instruction_prompt_pins() {
            for pin in table {
                assert!(ids.insert(pin.id()), "duplicate pin id {}", pin.id());
                assert!(
                    digests.insert(PromptDigest::of(pin.text())),
                    "{} pins a string that is already pinned",
                    pin.id()
                );
            }
        }
        assert!(ids.len() >= 27, "{} pins", ids.len());
    }
}
