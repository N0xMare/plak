//! Model-visible messages the Python worker writes into cell results.

#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact py-ptc tool identifiers"
)]

/// Python PTC exec_py cell, prefix before the worker detail when a raised
/// exception arrived with an undecodable traceback payload.
pub(crate) const PLAKOUS_PY_PTC_ERROR_UNDECODABLE_TRACEBACK_PREFIX: &str = "Python cell failed; its traceback frame spans were undecodable, so the exception text is \
     unavailable. The interpreter was reset, so names bound by earlier cells are gone. Worker \
     detail: ";

/// Python PTC exec_py cell, prefix before the nested-call limit in an outstanding-call refusal.
pub(crate) const PLAKOUS_PY_PTC_ERROR_OUTSTANDING_NESTED_PREFIX: &str =
    "local composition exceeds its profile-bound limit of ";

/// Python PTC exec_py cell, suffix after the nested-call limit in an outstanding-call refusal.
pub(crate) const PLAKOUS_PY_PTC_ERROR_OUTSTANDING_NESTED_SUFFIX: &str = " outstanding nested calls";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_prompts_digest_is_stable() {
        let combined = [
            PLAKOUS_PY_PTC_ERROR_UNDECODABLE_TRACEBACK_PREFIX,
            PLAKOUS_PY_PTC_ERROR_OUTSTANDING_NESTED_PREFIX,
            PLAKOUS_PY_PTC_ERROR_OUTSTANDING_NESTED_SUFFIX,
        ]
        .concat();
        assert_eq!(combined.len(), 264);
        assert!(combined.starts_with("Python cell failed"));
        assert!(!combined.contains("wait_py"));
    }
}
