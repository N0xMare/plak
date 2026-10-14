//! Process-tool-composition harness instructions.

use plakous_model::ToolName;
#[cfg(test)]
use plakous_tools::PromptPin;

/// JS PTC harness base instruction template; `{nested_tools}` lists nested cell names.
pub const PLAK_JS_PTC_HARNESS_TEMPLATE: &str = "You are a precise and proficient coding agent using JavaScript tool composition. The complete provider-visible tool set is exec_js and wait_js. Nested tools available inside cells: {nested_tools}. Direct calls to these names are unavailable. exec_js requires JSON field source (string), not code. The source is an async module: do not use a top-level return; emit visible output with text(...). Nested coding tools live only on the global tools object; call them as await tools.name({...}). If the user supplies JavaScript or asks to evaluate it, call exec_js with that source; do not echo the source as assistant text. If exec_js returns a running cell_id, call wait_js with that cell_id until completed. Use only the tools provided; do not invent tools that are not presented.";

/// Python PTC harness base instruction template; `{nested_tools}` lists nested cell names.
pub const PLAK_PY_PTC_HARNESS_TEMPLATE: &str = "You are a precise and proficient coding agent using Python tool composition. The complete provider-visible tool set is exec_py and wait_py. exec_py yields after yield_time_ms (default 10000, max 10000); resume a running cell with wait_py {cell_id, yield_time_ms?, terminate?} (default 10000, max 600000). Steers yield observation without killing the cell. Nested tools available inside cells: {nested_tools}. Direct calls to these names are unavailable. exec_py requires JSON field code (string), not source. Nested coding tools are injected async functions inside the cell; call structured tools as await name({...}) and freeform tools with the string shown in their signature. Tool results are strings. Calling without awaiting or scheduling creates an inert coroutine; use import asyncio and await asyncio.gather(...) for concurrent calls. If the user supplies Python or asks to evaluate it, call exec_py with that code; do not echo the code as assistant text. Printed lines and the last expression are both visible in the tool result. Cells cannot open files, read the environment, use the network or sleep (those calls raise); use the nested tools for workspace effects. Use only the tools provided; do not invent tools that are not presented.";

/// The two process-tool-composition harnesses, each with its own base
/// instruction and its own nested-call syntax.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PtcHarness {
    /// `exec_js`/`wait_js`, nested tools on the global `tools` object.
    JavaScript,
    /// `exec_py`/`wait_py`, nested tools injected as async functions.
    Python,
}

impl PtcHarness {
    /// The base instruction template; `{nested_tools}` is the catalog list.
    #[must_use]
    pub const fn template(self) -> &'static str {
        match self {
            Self::JavaScript => PLAK_JS_PTC_HARNESS_TEMPLATE,
            Self::Python => PLAK_PY_PTC_HARNESS_TEMPLATE,
        }
    }

    /// The base instruction listing the nested names of one compiled surface,
    /// in surface order.
    #[must_use]
    pub fn instructions(self, nested_names: &[ToolName]) -> String {
        self.template().replace(
            "{nested_tools}",
            &nested_names
                .iter()
                .map(ToolName::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

/// Every model-visible string this module owns.
#[cfg(test)]
pub static PTC_PROMPT_PINS: &[PromptPin] = &[
    PromptPin::new(
        "ptc.js_ptc_harness_template",
        PLAK_JS_PTC_HARNESS_TEMPLATE,
        "820545621080dbfb79145e35910d2373d253829381e02201f7debcb6b8b3c3fb",
    ),
    PromptPin::new(
        "ptc.py_ptc_harness_template",
        PLAK_PY_PTC_HARNESS_TEMPLATE,
        "7499c82617900df56ace5f98ad5a28ee698f8d99795668aa04c649808ab24394",
    ),
];

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests {
    use super::*;
    use plakous_compose::standard_coding_menus;
    use plakous_model::ToolName;
    use plakous_tools::{CodingPresentation, menu_names};

    /// The nested catalog for a Codex node with no extension: the compiled
    /// menu, not a table.
    fn codex_catalog() -> Vec<ToolName> {
        menu_names(
            &standard_coding_menus(plakous_tools::JobConfig::default()),
            CodingPresentation::Codex,
        )
        .expect("codex catalog")
    }

    #[test]
    fn js_ptc_harness_instructions_list_nested_catalog_names() {
        let catalog = codex_catalog();
        let instructions = PtcHarness::JavaScript.instructions(&catalog);
        assert!(instructions.contains("Nested tools available inside cells:"));
        for name in &catalog {
            assert!(instructions.contains(name.as_str()));
        }
        assert!(!instructions.contains("read_file, search_repository"));
    }

    #[test]
    fn py_ptc_harness_instructions_list_nested_catalog_names() {
        let catalog = codex_catalog();
        let instructions = PtcHarness::Python.instructions(&catalog);
        assert!(instructions.contains("Nested tools available inside cells:"));
        for name in &catalog {
            assert!(instructions.contains(name.as_str()));
        }
        assert!(instructions.contains("wait_py"));
    }

    #[test]
    fn ptc_harness_templates_are_stable() {
        assert!(PLAK_JS_PTC_HARNESS_TEMPLATE.starts_with("You are a precise"));
        assert!(!PLAK_JS_PTC_HARNESS_TEMPLATE.contains('&'));
        assert!(PLAK_PY_PTC_HARNESS_TEMPLATE.starts_with("You are a precise"));
        assert!(PLAK_PY_PTC_HARNESS_TEMPLATE.contains("wait_py"));
        assert!(!PLAK_PY_PTC_HARNESS_TEMPLATE.contains('&'));
    }
}
