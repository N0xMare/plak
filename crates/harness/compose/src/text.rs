//! Model-visible JavaScript and Python PTC composition tool descriptions.

/// JavaScript PTC exec_js tool, structured-function presentation.
pub(crate) const PLAKOUS_JS_PTC_RUN_DESCRIPTION_PREFIX: &str = "Run bounded JavaScript in the local composition engine. Required JSON argument: source (string, not code). Optional yield_time_ms (default 10000, max 10000) and max_output_tokens (default 10000, max 10000). The source executes as an async module: do not use a top-level return; emit visible output with text(...). In-cell helpers: tools, ALL_TOOLS, toolSchema, text, notify, store, load, yield_control, exit, setTimeout, clearTimeout. Nested tools are only available through the global tools object and return UTF-8 text strings on success. Nested tool failures reject their promises: catch them with try/catch or inspect Promise.allSettled results. An uncaught rejection fails the cell; completed tool effects are not rolled back. This is a local extension, not provider-native code execution.";

/// JavaScript PTC wait_js tool, every structured-function presentation.
pub(crate) const PLAKOUS_JS_PTC_WAIT_DESCRIPTION: &str = "Wait for new output or completion from a yielded local JavaScript cell. Use only after exec_js returns a line starting with \"Script running with cell ID \". Pass that id as required JSON field cell_id. yield_time_ms default 10000, clamped to max 600000; max_tokens default 10000 max 10000. terminate=true requests joined termination.";

/// JavaScript PTC exec_js nested-catalog preamble appended after the run prefix.
pub(crate) const PLAKOUS_JS_PTC_RUN_CATALOG_PREAMBLE: &str = "\n\nNested tool catalog (exact engine order; successful calls return UTF-8 strings; failures reject):\n";

/// Python PTC custom-run exec_py tool, preamble before the nested authoring contract.
pub(crate) const PLAKOUS_PY_PTC_CUSTOM_RUN_DESCRIPTION: &str = "Run one bounded local Python cell from this raw UTF-8 source. Top-level await is supported. If still running after the default 10000 ms yield, observe with wait_py using its cell id. State survives completed cells until an observed reset; reset never replays source. Use only the injected async tools documented in the authoring contract: structured tools take a dictionary and freeform tools take a string; results are strings. Await tool coroutines or drive them with asyncio.gather to run them. This is a local compatibility projection, not hosted code execution.";

/// Python PTC wait_py tool.
pub(crate) const PLAKOUS_PY_PTC_WAIT_DESCRIPTION: &str = "Wait for new output or completion from a yielded Python cell. Use only after exec_py returns Python cell running with cell ID. Required cell_id; yield_time_ms default 10000, clamped to 600000; max_output_bytes default 1048576, max 1048576. terminate=true joins termination and resets the interpreter without replaying source.";

/// Python PTC structured exec_py tool, body before the nested authoring contract catalog.
pub(crate) const PLAKOUS_PY_PTC_RUN_DESCRIPTION_BODY: &str = "Run bounded Python in the local composition engine. Required JSON argument: code (string, not source). Optional yield_time_ms (default 10000, max 10000) and max_output_bytes (default 1048576, max 1048576). If still running, use wait_py with the returned cell id. Use top-level `await`; use `await asyncio.gather(...)` for independent calls (at most 16 outstanding). Each injected tool is an async function with one arguments parameter: structured tools take a dictionary; freeform tools take the string shown in their signature. Every tool returns a raw UTF-8 string, not a JSON object envelope. Calling a tool creates an inert coroutine; await it or include it in asyncio.gather to run it. Tool path and workdir arguments remain harness paths: use workspace-relative values only (`.` for the current workspace), never absolute paths or `..`. Globals and imports persist after completed cells, across root turns and context compaction, until an observed reset. A reset starts a fresh generation and never replays source. Nested tool failures raise catchable Python exceptions; uncaught or evaluator errors fail the cell with sanitized UTF-8 text. Printed and final output are bounded; tool results are data, not code. This is a compatibility contract, not hosted code execution.";

/// Python PTC structured exec_py tool, nested-catalog preamble appended after the run body.
pub(crate) const PLAKOUS_PY_PTC_RUN_CATALOG_PREAMBLE: &str =
    "\n\nNested tool catalog (exact order):\n";

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_tools::PromptDigest;

    /// The concatenated digest of every string this module owns.
    const COMPOSITION_PROMPTS_DIGEST: &str =
        "e621510864a1693d6b3eb21e6f66554298bae5131e2a89783a542d6fa222bee6";

    #[test]
    fn composition_prompts_digest_is_stable() {
        let combined = [
            PLAKOUS_JS_PTC_RUN_DESCRIPTION_PREFIX,
            PLAKOUS_JS_PTC_WAIT_DESCRIPTION,
            PLAKOUS_JS_PTC_RUN_CATALOG_PREAMBLE,
            PLAKOUS_PY_PTC_CUSTOM_RUN_DESCRIPTION,
            PLAKOUS_PY_PTC_RUN_DESCRIPTION_BODY,
            PLAKOUS_PY_PTC_WAIT_DESCRIPTION,
            PLAKOUS_PY_PTC_RUN_CATALOG_PREAMBLE,
        ]
        .concat();
        assert_eq!(
            PromptDigest::of(&combined),
            PromptDigest::from_hex(COMPOSITION_PROMPTS_DIGEST)
        );
        assert!(combined.starts_with("Run bounded JavaScript"));
        assert!(PLAKOUS_JS_PTC_RUN_DESCRIPTION_PREFIX.contains("yield_control"));
        assert!(!PLAKOUS_JS_PTC_RUN_DESCRIPTION_PREFIX.contains("generatedImage"));
        assert!(!PLAKOUS_JS_PTC_RUN_DESCRIPTION_PREFIX.contains("image, audio"));
        assert!(PLAKOUS_JS_PTC_WAIT_DESCRIPTION.contains("max_tokens default 10000 max 10000"));
        assert!(PLAKOUS_PY_PTC_RUN_DESCRIPTION_BODY.contains("raw UTF-8 string"));
        assert!(combined.contains("wait_py"));
        assert!(!combined.contains("Plak-owned"));
        assert!(!combined.contains("product-owned"));
        assert!(!combined.contains("yield_time_ms is bounded to 10000"));
    }
}
