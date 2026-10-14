//! Model-visible agent runtime observation and composition status text.

#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact provider tool identifiers"
)]

/// Runtime turn loop, returned when the observation budget is exhausted for tool output.
pub(crate) const PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED: &str = "Tool output was withheld for context capacity. The harness compacts automatically when enabled; retry with a smaller read or a narrower query afterwards.";

/// Runtime turn loop, the line that replaces the middle of a clipped tool
/// observation, rendered as `[<N> <this text>]` between the kept head and tail.
pub(crate) const PLAKOUS_RUNTIME_OBSERVATION_OMITTED: &str =
    "bytes omitted from the middle of this output to fit the context budget";

/// JavaScript PTC result projection, yielded exec_js cell status line prefix.
pub(crate) const PLAKOUS_RUNTIME_SCRIPT_STATUS_RUNNING_PREFIX: &str =
    "Script running with cell ID ";

/// JavaScript PTC result projection, terminated exec_js cell status line.
pub(crate) const PLAKOUS_RUNTIME_SCRIPT_STATUS_TERMINATED: &str = "Script terminated";

/// JavaScript PTC result projection, failed exec_js cell status line.
pub(crate) const PLAKOUS_RUNTIME_SCRIPT_STATUS_FAILED: &str = "Script failed";

/// JavaScript PTC result projection, completed exec_js cell status line.
pub(crate) const PLAKOUS_RUNTIME_SCRIPT_STATUS_COMPLETED: &str = "Script completed";

/// Python PTC result projection, yielded exec_py cell status line prefix.
pub(crate) const PLAKOUS_RUNTIME_PYTHON_CELL_STATUS_RUNNING_PREFIX: &str =
    "Python cell running with cell ID ";

/// Python PTC result projection, terminated exec_py cell status line.
pub(crate) const PLAKOUS_RUNTIME_PYTHON_CELL_STATUS_TERMINATED: &str = "Python cell terminated";

/// Python PTC result projection, failed exec_py cell status line.
pub(crate) const PLAKOUS_RUNTIME_PYTHON_CELL_STATUS_FAILED: &str = "Python cell failed";

/// Python PTC result projection, completed exec_py cell status line.
pub(crate) const PLAKOUS_RUNTIME_PYTHON_CELL_STATUS_COMPLETED: &str = "Python cell completed";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_prompts_digest_is_stable() {
        let combined = [
            PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED,
            PLAKOUS_RUNTIME_OBSERVATION_OMITTED,
            PLAKOUS_RUNTIME_SCRIPT_STATUS_RUNNING_PREFIX,
            PLAKOUS_RUNTIME_SCRIPT_STATUS_TERMINATED,
            PLAKOUS_RUNTIME_SCRIPT_STATUS_FAILED,
            PLAKOUS_RUNTIME_SCRIPT_STATUS_COMPLETED,
            PLAKOUS_RUNTIME_PYTHON_CELL_STATUS_RUNNING_PREFIX,
            PLAKOUS_RUNTIME_PYTHON_CELL_STATUS_TERMINATED,
            PLAKOUS_RUNTIME_PYTHON_CELL_STATUS_FAILED,
            PLAKOUS_RUNTIME_PYTHON_CELL_STATUS_COMPLETED,
        ]
        .concat();
        assert_eq!(combined.len(), 391);
        assert!(combined.starts_with("Tool output was withheld for context capacity"));
        assert!(PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED.contains("smaller read"));
        assert!(
            PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED.contains("compacts automatically when enabled")
        );
        assert!(!PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED.contains("/compact"));
        assert_eq!(
            plakous_model::hex_lower(&plakous_model::sha256_bytes(&[
                PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED.as_bytes()
            ])),
            "ed68fb57a2c771d5b4013788e82daa7be8aff9a093bfdc00261db99141abd95c"
        );
    }
}
