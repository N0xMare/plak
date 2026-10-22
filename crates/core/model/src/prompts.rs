//! Model-visible compaction and related instruction text.

#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact provider tool identifiers"
)]

/// Driver-owned no-tools semantic compaction operation, trailing user instruction.
pub const PLAKOUS_SEMANTIC_COMPACTION_INSTRUCTION: &str = "Summarize the conversation above so the work can continue from your summary alone. The summary replaces the conversation, including any earlier summary, in your own working context; it is not a reply to the user, so instructions about what a reply may show do not apply to it. Begin with the exact line `Summary of earlier work:` on its own, then cover, briefly and with exact names:\n- each user request in order, quoting its requirements verbatim; this instruction is not one of them. A request you answered with a final reply is done, apart from anything that reply left undone; say so for a request that was interrupted or never answered. If the latest request above is still in progress, it is the active one and is kept verbatim after this summary;\n- every value the remaining work still needs, copied exactly: identifiers, tokens, numbers, short file contents and text to write or report later;\n- work already done, as the tool results above show it, stated as done so it is not repeated: files read or written, with paths, and commands run with their results, including failures;\n- decisions made and why;\n- errors still open;\n- the plan or todo list, with each item's status;\n- the next step.\nCarry forward everything from an earlier summary that still applies. Return only the summary text; do not call tools, continue the conversation, or add commentary.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_prompt_digest_is_stable() {
        let combined = PLAKOUS_SEMANTIC_COMPACTION_INSTRUCTION.to_owned();
        assert_eq!(combined.len(), 1364);
        assert!(combined.starts_with("Summarize the conversation above"));
        assert!(
            combined.contains("Begin with the exact line `Summary of earlier work:` on its own")
        );
        assert!(combined.contains("every value the remaining work still needs, copied exactly"));
        assert!(combined.contains("this instruction is not one of them"));
        assert!(combined.contains("stated as done so it is not repeated"));
        assert!(combined.contains("If the latest request above is still in progress"));
    }
}
