//! Model-visible compaction and related instruction text.

#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact provider tool identifiers"
)]

/// Driver-owned no-tools semantic compaction operation, trailing user instruction.
pub const PLAKOUS_SEMANTIC_COMPACTION_INSTRUCTION: &str = "Summarize the conversation above so the work can continue from your summary alone. Begin with the exact line `Summary of earlier work:` on its own, then cover, briefly and with exact names:\n- the user's goal and constraints, quoting their requirements verbatim;\n- decisions made and why;\n- files read or changed, with paths;\n- commands run and their results, including failures;\n- errors still open;\n- the plan or todo list, with each item's status;\n- the next step.\nReturn only the summary text; do not call tools, continue the conversation, or add commentary.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_prompt_digest_is_stable() {
        let combined = PLAKOUS_SEMANTIC_COMPACTION_INSTRUCTION.to_owned();
        assert_eq!(combined.len(), 561);
        assert!(combined.starts_with("Summarize the conversation above"));
        assert!(
            combined.contains("Begin with the exact line `Summary of earlier work:` on its own")
        );
    }
}
