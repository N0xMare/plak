use super::host_api::HostApi;
use crate::text::{PLAKOUS_PY_PTC_RUN_CATALOG_PREAMBLE, PLAKOUS_PY_PTC_RUN_DESCRIPTION_BODY};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
fn render_python_type(schema: &Value) -> String {
    let Some(object) = schema.as_object() else {
        return "object".to_owned();
    };
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        let literals = values
            .iter()
            .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "object".to_owned()))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("Literal[{literals}]");
    }
    match object.get("type").and_then(Value::as_str) {
        Some("string") => "str".to_owned(),
        Some("integer") => "int".to_owned(),
        Some("number") => "float".to_owned(),
        Some("boolean") => "bool".to_owned(),
        Some("null") => "None".to_owned(),
        Some("array") => object.get("items").map_or_else(
            || "list".to_owned(),
            |items| format!("list[{}]", render_python_type(items)),
        ),
        Some("object") | None
            if object.contains_key("properties") || object.contains_key("additionalProperties") =>
        {
            render_python_dict_type(object)
        }
        _ => "object".to_owned(),
    }
}

fn render_python_dict_type(object: &serde_json::Map<String, Value>) -> String {
    let empty = serde_json::Map::new();
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let required = object
        .get("required")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let mut properties = properties.iter().collect::<Vec<_>>();
    properties.sort_unstable_by_key(|(name, _)| *name);
    let fields = properties
        .into_iter()
        .map(|(name, schema)| {
            let optional = if required.contains(name.as_str()) {
                ""
            } else {
                "?"
            };
            let comment = schema
                .as_object()
                .and_then(|property| property.get("description"))
                .and_then(Value::as_str)
                .map_or_else(String::new, |description| format!("  # {description}"));
            format!(
                "  \"{name}\"{optional}: {},{comment}",
                render_python_type(schema)
            )
        })
        .collect::<Vec<_>>();
    if fields.is_empty() {
        "dict".to_owned()
    } else {
        format!("{{\n{}\n}}", fields.join("\n"))
    }
}

pub(super) fn normalize_python_identifier(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len() + 1);
    for (index, character) in name.chars().enumerate() {
        let valid = character == '_' || character.is_ascii_alphanumeric();
        if index == 0 && character.is_ascii_digit() {
            normalized.push('_');
        }
        normalized.push(if valid { character } else { '_' });
    }
    if PYTHON_KEYWORDS.contains(&normalized.as_str()) {
        normalized.push('_');
    }
    normalized
}

const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

pub(super) fn render_pyi(api: &HostApi) -> String {
    let tools = &api.namespaces[0].functions;
    let catalog = tools
        .iter()
        .map(|tool| {
            let result_type = if tool.returns_object() { "dict" } else { "str" };
            let signature = if tool.string_input() {
                format!(
                    "async def {}(arguments: str) -> {result_type}",
                    tool.python_name
                )
            } else {
                let arguments = tool.input_schema().map_or_else(
                    || "dict".to_owned(),
                    |nested_schema| render_python_type(nested_schema.document().value()),
                );
                format!(
                    "async def {}(arguments: {arguments}) -> {result_type}",
                    tool.python_name
                )
            };
            format!(
                "- `{}`: {}\n  `{signature}`",
                tool.exposed_name, tool.description,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{PLAKOUS_PY_PTC_RUN_DESCRIPTION_BODY}{PLAKOUS_PY_PTC_RUN_CATALOG_PREAMBLE}{catalog}")
}
const HOST_PREFIX: &str = "_plakous_host_";
const MAX_BINDINGS: usize = 64;
const MAX_BOOTSTRAP_BYTES: usize = 65_536;
const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Interpreter-neutral input representation for one Python host function.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PythonInput {
    /// One structured dictionary argument.
    Dictionary,
    /// One raw string argument.
    String,
}

/// One generated private Python function and its host-side tool binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PythonBootstrapBinding {
    /// Private coroutine target emitted in the bootstrap.
    pub function: String,
    /// Canonical exposed name used by nested dispatch.
    pub tool: String,
    /// Advertised argument representation.
    pub input: PythonInput,
}

/// Generated Python source and routing data, independent of a worker protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PythonBootstrap {
    /// Exact source run before the first cell.
    pub source: String,
    /// Ordered private host-function bindings.
    pub bindings: Vec<PythonBootstrapBinding>,
}

pub(super) struct BootstrapTool {
    pub tool_name: String,
    pub input_kind: PythonInput,
}

/// The bootstrap that defines one coroutine per tool, or `None` for an empty
/// catalog, which needs none.
fn bootstrap_bindings(
    bindings: BTreeMap<String, BootstrapTool>,
) -> Result<Option<PythonBootstrap>, String> {
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
        host.push(PythonBootstrapBinding {
            function: private_name,
            tool: tool.tool_name,
            input: tool.input_kind,
        });
    }
    Ok((!host.is_empty()).then_some(PythonBootstrap {
        source,
        bindings: host,
    }))
}

/// Generates the bounded Monty coroutine bootstrap from the host API.
///
/// # Errors
/// Rejects reserved/invalid Python exports and catalog or source bounds.
pub fn python_bootstrap(api: &HostApi) -> Result<Option<PythonBootstrap>, String> {
    if let Some(sdk) = &api.sdk {
        return Ok(Some(sdk.bootstrap()));
    }
    bootstrap_bindings(
        api.namespaces
            .iter()
            .flat_map(|namespace| &namespace.functions)
            .map(|tool| {
                (
                    tool.python_name.clone(),
                    BootstrapTool {
                        tool_name: tool.exposed_name.as_str().to_owned(),
                        input_kind: if tool.string_input() {
                            PythonInput::String
                        } else {
                            PythonInput::Dictionary
                        },
                    },
                )
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> BootstrapTool {
        BootstrapTool {
            tool_name: "tool".into(),
            input_kind: PythonInput::Dictionary,
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
            assert!(bootstrap_bindings(BTreeMap::from([(name.into(), tool())])).is_err());
        }
    }

    #[test]
    fn bootstrap_bounds_apply_before_starting_a_worker() {
        let excess = (0..=MAX_BINDINGS)
            .map(|index| (format!("tool_{index}"), tool()))
            .collect();
        assert!(bootstrap_bindings(excess).is_err());
        assert!(
            bootstrap_bindings(BTreeMap::from([("x".repeat(MAX_BOOTSTRAP_BYTES), tool(),)]))
                .is_err()
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
        assert!(bootstrap_bindings(aggregate).is_err());
    }

    #[test]
    fn empty_catalog_needs_no_bootstrap() -> Result<(), String> {
        assert_eq!(bootstrap_bindings(BTreeMap::new())?, None);
        Ok(())
    }
}
