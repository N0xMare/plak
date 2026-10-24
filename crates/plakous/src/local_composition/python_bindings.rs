//! Persistent Python coroutine wrappers over the trusted host-call allowlist:
//! the bootstrap a Monty session runs before its first cell.

use std::collections::BTreeMap;

use plakous_cell::{BindingInput, MontyBinding, MontyBootstrap};

const HOST_PREFIX: &str = "_plakous_host_";
const MAX_BINDINGS: usize = 64;
const MAX_BOOTSTRAP_BYTES: usize = 65_536;
const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Trusted routing and input contract for one Python tool wrapper.
#[derive(Clone)]
pub(super) struct PythonToolBinding {
    /// Canonical exposed name used by nested tool dispatch.
    pub(super) tool_name: String,
    /// Input representation advertised by the tool catalog.
    pub(super) input_kind: BindingInput,
}

/// The bootstrap that defines one coroutine per tool, or `None` for an empty
/// catalog, which needs none.
pub(super) fn python_bootstrap(
    bindings: BTreeMap<String, PythonToolBinding>,
) -> Result<Option<MontyBootstrap>, String> {
    if bindings.len() > MAX_BINDINGS {
        return Err("Python tool catalog exceeds 64 bindings".into());
    }
    let mut source = String::new();
    let mut host = Vec::new();
    for (index, (name, tool)) in bindings.into_iter().enumerate() {
        let mut bytes = name.bytes();
        let valid_start = bytes
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic());
        if !valid_start
            || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
            || KEYWORDS.contains(&name.as_str())
            || name.starts_with(HOST_PREFIX)
        {
            return Err("Python tool name must be a non-reserved ASCII identifier".into());
        }
        // Validate before formatting: caller-supplied names never become
        // arbitrary Python source, nor an unbounded bootstrap allocation.
        if name.len() > MAX_BOOTSTRAP_BYTES {
            return Err("Python tool bootstrap exceeds 64 KiB".into());
        }
        let private_name = format!("{HOST_PREFIX}{index}");
        let definition =
            format!("async def {name}(arguments):\n    return await {private_name}(arguments)\n");
        if source.len().saturating_add(definition.len()) > MAX_BOOTSTRAP_BYTES {
            return Err("Python tool bootstrap exceeds 64 KiB".into());
        }
        source.push_str(&definition);
        host.push(MontyBinding {
            function: private_name,
            tool: tool.tool_name,
            input: tool.input_kind,
        });
    }
    Ok((!host.is_empty()).then_some(MontyBootstrap {
        source,
        bindings: host,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> PythonToolBinding {
        PythonToolBinding {
            tool_name: "tool".into(),
            input_kind: BindingInput::Dictionary,
        }
    }

    #[test]
    fn invalid_exports_cannot_inject_source_or_replace_private_bindings() {
        for name in [
            "",
            "await",
            "1tool",
            "tool-name",
            "tool():\n    print('injected')\nasync def other",
            "_plakous_host_0",
            "_plakous_host_anything",
            "tøøl",
        ] {
            assert!(python_bootstrap(BTreeMap::from([(name.into(), tool())])).is_err());
        }
    }

    #[test]
    fn bootstrap_bounds_apply_before_starting_a_worker() {
        let excess = (0..=MAX_BINDINGS)
            .map(|index| (format!("tool_{index}"), tool()))
            .collect();
        assert!(python_bootstrap(excess).is_err());
        assert!(
            python_bootstrap(BTreeMap::from([("x".repeat(MAX_BOOTSTRAP_BYTES), tool(),)])).is_err()
        );
        let aggregate = (0..MAX_BINDINGS)
            .map(|index| {
                (
                    format!(
                        "tool_{index}_{}",
                        "x".repeat(MAX_BOOTSTRAP_BYTES / MAX_BINDINGS)
                    ),
                    tool(),
                )
            })
            .collect();
        assert!(python_bootstrap(aggregate).is_err());
    }

    #[test]
    fn empty_catalog_needs_no_bootstrap() -> Result<(), String> {
        assert_eq!(python_bootstrap(BTreeMap::new())?, None);
        Ok(())
    }
}
