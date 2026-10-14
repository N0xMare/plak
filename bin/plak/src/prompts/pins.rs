//! One walker over the prompt pins that stay with the topology tools.
//!
//! The instruction text and its pins moved to `plakous-prompts`;
//! the topology text moved beside its tools in `agent_tree::text`. The rule is
//! unchanged either way — a pin table lives with its text, and this is the only
//! thing that hashes.

/// Every topology prompt pin table, in module order.
#[cfg(test)]
pub(crate) fn topology_prompt_pins() -> Vec<&'static [plakous::tools::PromptPin]> {
    vec![plakous_hosted::agent_tree::text::TOPOLOGY_PROMPT_PINS]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use plakous::tools::{PromptDigest, drifted_pins};

    use super::topology_prompt_pins;

    #[test]
    fn every_product_prompt_pin_matches_its_text() {
        let mut drifted = Vec::new();
        for table in topology_prompt_pins() {
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
        for table in topology_prompt_pins() {
            for pin in table {
                assert!(ids.insert(pin.id()), "duplicate pin id {}", pin.id());
                assert!(
                    digests.insert(PromptDigest::of(pin.text())),
                    "{} pins a string that is already pinned",
                    pin.id()
                );
            }
        }
        assert!(ids.len() >= 14, "{} pins", ids.len());
    }
}
