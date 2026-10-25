//! Model-visible compaction and related instruction text.

#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact provider tool identifiers"
)]

/// Driver-owned no-tools semantic compaction operation, trailing user instruction.
pub const PLAKOUS_SEMANTIC_COMPACTION_INSTRUCTION: &str = "Summarize the conversation above so the work can continue from your summary alone. The summary replaces the conversation, including any earlier summary, in your own working context; it is not a reply to the user, so instructions about what a reply may show do not apply to it. Begin with the exact line `Summary of earlier work:` on its own, then write each section below once, in this order, with exact names:\nRequests: the user's requests, oldest first; this instruction is not one of them. Quote the three newest requests that are not in progress verbatim, up to about 150 words each; for a longer request, quote up to that limit and keep every requirement in the user's own words. Give each earlier request one line, and merge earlier finished requests into at most 10 lines. Mark each request done, interrupted, unanswered or in progress. For a done request, copy exactly the values it produced: what it wrote, reported or answered. If its line cannot hold those values, put them in Values; never drop values to meet a line cap. A request still in progress continues verbatim after this summary; name it without quoting it again.\nConstraints: every rule, preference or requirement the user set in any request that still applies, quoted exactly. List each one here even if it is also quoted under Requests.\nValues: every value the remaining work still needs, copied exactly: identifiers, tokens, numbers, short file contents and text to write or report later.\nWork done: files read or changed with paths, commands run and their results, including failures; one line each, so finished work is not repeated.\nDecisions: each decision and why.\nOpen errors: each error still open.\nPlan: each item with its status.\nNext step: the next action, or the question awaiting the user's answer.\nMerge an earlier summary into these sections rather than copying it: keep what still applies, and shorten finished requests to their one-line entries. Keep the whole summary under about 2,000 words. Write each item once; never repeat a request, a section or a list. Return only the summary text; do not call tools, continue the conversation, or add commentary.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_prompt_digest_is_stable() {
        let combined = PLAKOUS_SEMANTIC_COMPACTION_INSTRUCTION.to_owned();
        assert_eq!(combined.len(), 2145);
        assert!(combined.starts_with("Summarize the conversation above"));
        assert!(
            combined.contains("Begin with the exact line `Summary of earlier work:` on its own")
        );
        assert!(combined.contains("every value the remaining work still needs, copied exactly"));
        assert!(combined.contains(
            "Quote the three newest requests that are not in progress verbatim, up to about 150 words each"
        ));
        assert!(combined.contains("merge earlier finished requests into at most 10 lines"));
        assert!(combined.contains("For a done request, copy exactly the values it produced"));
        assert!(combined.contains("for a longer request, quote up to that limit and keep every requirement in the user's own words"));
        assert!(combined.contains("Constraints: every rule, preference or requirement the user set in any request that still applies, quoted exactly."));
        assert!(combined.contains("List each one here even if it is also quoted under Requests."));
        assert!(
            combined.contains(
                "Next step: the next action, or the question awaiting the user's answer."
            )
        );
        assert!(combined.contains("If its line cannot hold those values, put them in Values; never drop values to meet a line cap."));
        assert!(combined.contains("Keep the whole summary under about 2,000 words."));
        assert!(
            combined.contains("Write each item once; never repeat a request, a section or a list.")
        );
        for section in [
            "\nRequests: ",
            "\nConstraints: ",
            "\nValues: ",
            "\nWork done: ",
            "\nDecisions: ",
            "\nOpen errors: ",
            "\nPlan: ",
            "\nNext step: ",
        ] {
            assert!(combined.contains(section), "{section}");
        }
        assert!(!combined.contains("Carry forward everything"));
    }
}
