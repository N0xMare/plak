use super::host_api::HostApi;
use super::presentation::CatalogStyle;
use crate::text::{PLAKOUS_JS_PTC_RUN_CATALOG_PREAMBLE, PLAKOUS_JS_PTC_RUN_DESCRIPTION_PREFIX};
use serde_json::{Value, json};
use std::collections::BTreeSet;
const EXACT_REFERENCE_EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run JavaScript code to orchestrate/compose tool calls
- Evaluates the provided JavaScript code in a fresh V8 isolate as an async module.
- All nested tools are available on the global `tools` object, for example `await tools.exec_command(...)`. Tool names are exposed as normalized JavaScript identifiers.
- Nested tool methods take either a string or an object as their input argument.
- Nested tools return either an object or a string, based on the description.
- Runs raw JavaScript -- no Node, no file system, no network access, no console.
- Accepts raw JavaScript source text, not JSON, quoted strings, or markdown code fences.
- You may optionally start the tool input with a first-line pragma like `// @exec: {"yield_time_ms": 10000, "max_output_tokens": 1000}`.
- `yield_time_ms` asks `exec` to yield early if the script is still running. Defaults to 10000 ms.
- `max_output_tokens` sets the token budget for direct `exec` results. Defaults to 10000 tokens.
- When the JS code is fully evaluated, the isolate's lifetime ends and unawaited promises are silently discarded.

- Global helpers:
- `exit()`: Immediately ends the current script successfully (like an early return from the top level).
- `text(value: string | number | boolean | undefined | null)`: Appends a text item. Non-string values are stringified with `JSON.stringify(...)` when possible.
- `store(key: string, value: any)`: stores a serializable value under a string key for later `exec` calls in the same session.
- `load(key: string)`: returns the stored value for a string key, or `undefined` if it is missing.
- `notify(value: string | number | boolean | undefined | null)`: immediately injects an extra `custom_tool_call_output` for the current `exec` call. Values are stringified like `text(...)`.
- `setTimeout(callback: () => void, delayMs?: number)`: schedules a callback to run later and returns a timeout id. Pending timeouts do not keep `exec` alive by themselves; await an explicit promise if you need to wait for one.
- `clearTimeout(timeoutId?: number)`: cancels a timeout created by `setTimeout`.
- `ALL_TOOLS`: metadata for the enabled nested tools as `{ name, description }` entries.
- `yield_control()`: yields the accumulated output to the model immediately while the script keeps running."#;
const MINIMAL_EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript as an async module in a fresh isolate (no Node, filesystem, network, or console). Supply source, not JSON, quoted strings, or markdown fences.
- All nested tools use the global `tools` object, e.g. `await tools.exec_command(...)`; names are normalized JavaScript identifiers. Inputs and results are strings or objects as documented.
- Optional first-line pragma: `// @exec: {"yield_time_ms": 10000, "max_output_tokens": 1000}`. `yield_time_ms` yields early while running (default 10000 ms); `max_output_tokens` bounds direct results (default 10000 tokens).
- On completion the isolate ends; unawaited promises are discarded.

Global helpers:
- `exit()`: immediately end successfully.
- `text(value: string | number | boolean | undefined | null)`: appends output. Non-string values are stringified with `JSON.stringify(...)` when possible.
- `store(key: string, value: any)` / `load(key: string)`: save serializable session values across cells; missing keys return `undefined`.
- `notify(value: string | number | boolean | undefined | null)`: immediately emits `custom_tool_call_output`, using `text` conversion.
- `setTimeout(callback: () => void, delayMs?: number)`: schedule a callback and return its timer id. Pending timers do not keep the isolate alive; await an explicit promise.
- `clearTimeout(timeoutId?: number)`: cancel a scheduled timer.
- `ALL_TOOLS`: enabled nested tool metadata `{ name, description }`.
- `yield_control()`: immediately yield accumulated output while the script continues."#;
const EXEC_DECLARATION_HEADER: &str =
    "Nested tool declarations, all members of `declare const tools`:";

const PROCESS_TOOL_RESULT_TYPE: &str =
    "Promise<{ output: string; exit_code?: number; session_id?: number; status?: string }>";

fn render_schema_type(schema: &Value) -> String {
    let Some(object) = schema.as_object() else {
        return "unknown".to_owned();
    };
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_owned()))
            .collect::<Vec<_>>()
            .join(" | ");
    }
    match object.get("type").and_then(Value::as_str) {
        Some("string") => "string".to_owned(),
        Some("integer" | "number") => "number".to_owned(),
        Some("boolean") => "boolean".to_owned(),
        Some("null") => "null".to_owned(),
        Some("array") => object.get("items").map_or_else(
            || "unknown[]".to_owned(),
            |items| format!("Array<{}>", render_schema_type(items)),
        ),
        Some("object") | None
            if object.contains_key("properties") || object.contains_key("additionalProperties") =>
        {
            render_object_type(object)
        }
        _ => "unknown".to_owned(),
    }
}

fn render_object_type(object: &serde_json::Map<String, Value>) -> String {
    let empty = serde_json::Map::new();
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let has_descriptions = properties.values().any(|schema| {
        schema
            .as_object()
            .and_then(|property| property.get("description"))
            .is_some()
    });
    if has_descriptions {
        render_commented_object_type(object)
    } else {
        render_inline_object_type(object)
    }
}

fn render_inline_object_type(object: &serde_json::Map<String, Value>) -> String {
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
            let rendered_name = if normalize_javascript_identifier(name) == *name {
                name.clone()
            } else {
                serde_json::to_string(name).unwrap_or_else(|_| format!("\"{name}\""))
            };
            format!("{rendered_name}{optional}: {};", render_schema_type(schema))
        })
        .collect::<Vec<_>>();
    if fields.is_empty() {
        "{}".to_owned()
    } else {
        format!("{{ {} }}", fields.join(" "))
    }
}

fn render_commented_object_type(object: &serde_json::Map<String, Value>) -> String {
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
            let rendered_name = if normalize_javascript_identifier(name) == *name {
                name.clone()
            } else {
                serde_json::to_string(name).unwrap_or_else(|_| format!("\"{name}\""))
            };
            let comment = schema
                .as_object()
                .and_then(|property| property.get("description"))
                .and_then(Value::as_str)
                .map_or_else(
                    || "  ".to_owned(),
                    |description| format!("  // {description}\n  "),
                );
            format!(
                "{comment}{rendered_name}{optional}: {};",
                render_schema_type(schema)
            )
        })
        .collect::<Vec<_>>();
    if fields.is_empty() {
        "{}".to_owned()
    } else {
        format!("{{\n{}\n}}", fields.join("\n"))
    }
}

/// Selects ordered JavaScript globals, retaining exact names on alias collisions.
#[must_use]
pub fn javascript_globals<'a>(names: impl Iterator<Item = &'a str> + Clone) -> Vec<String> {
    let mut claims: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for name in names.clone() {
        *claims.entry(name.to_owned()).or_default() += 1;
        let alias = normalize_javascript_identifier(name);
        if alias != name {
            *claims.entry(alias).or_default() += 1;
        }
    }
    names
        .map(|name| {
            let alias = normalize_javascript_identifier(name);
            if claims.get(&alias) == Some(&1) {
                alias
            } else {
                name.to_owned()
            }
        })
        .collect()
}

/// Maps a tool name to the pinned Codex JavaScript identifier spelling.
#[must_use]
pub fn normalize_javascript_identifier(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    for (index, character) in name.chars().enumerate() {
        let valid = if index == 0 {
            character == '_' || character == '$' || character.is_ascii_alphabetic()
        } else {
            character == '_' || character == '$' || character.is_ascii_alphanumeric()
        };
        normalized.push(if valid { character } else { '_' });
    }
    if normalized.is_empty() {
        normalized.push('_');
    }
    normalized
}

fn render_codex(api: &HostApi, exact_reference: bool) -> String {
    let tools = &api.namespaces[0].functions;
    let template = if exact_reference {
        EXACT_REFERENCE_EXEC_DESCRIPTION_TEMPLATE
    } else {
        MINIMAL_EXEC_DESCRIPTION_TEMPLATE
    };
    let globals = javascript_globals(tools.iter().map(|tool| tool.exposed_name.as_str()));
    let nested = tools
        .iter()
        .zip(globals)
        .map(|(tool, global)| {
            let name = tool.exposed_name.as_str();
            let identifier = normalize_javascript_identifier(&global) == global;
            let member = if identifier {
                global.clone()
            } else {
                json!(global).to_string()
            };
            let heading = if global != name || !identifier {
                let call = if identifier {
                    format!("tools.{global}")
                } else {
                    format!("tools[{member}]")
                };
                format!("### `{name}` (call `{call}`)")
            } else {
                format!("### `{name}`")
            };
            let signature = if tool.string_input() {
                format!("{member}(input: string)")
            } else {
                let input = tool.input_schema().map_or_else(
                    || "unknown".to_owned(),
                    |schema| render_schema_type(schema.document().value()),
                );
                format!("{member}(args: {input})")
            };
            let return_type = if tool.returns_object() {
                PROCESS_TOOL_RESULT_TYPE
            } else {
                "Promise<unknown>"
            };
            format!(
                "{heading}\n{}\n\n```ts\n{signature}: {return_type};\n```",
                tool.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{template}\n\n{EXEC_DECLARATION_HEADER}\n\n{nested}")
}
fn render_product(api: &HostApi) -> String {
    let tools = &api.namespaces[0].functions;
    let catalog = tools
        .iter()
        .map(|tool| {
            let name = tool.exposed_name.as_str();
            let function = normalize_javascript_identifier(name);
            let result_type = if tool.returns_object() {
                PROCESS_TOOL_RESULT_TYPE
            } else {
                "Promise<string>"
            };
            let signature = if tool.string_input() {
                format!("tools.{function}(input: string): {result_type}")
            } else {
                let input = tool.input_schema().map_or_else(
                    || "unknown".to_owned(),
                    |schema| render_schema_type(schema.document().value()),
                );
                format!("tools.{function}(args: {input}): {result_type}")
            };
            format!("- `{name}`: {}\n  `{signature}`", tool.description)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{PLAKOUS_JS_PTC_RUN_DESCRIPTION_PREFIX}{PLAKOUS_JS_PTC_RUN_CATALOG_PREAMBLE}{catalog}")
}

pub(super) fn render_ts(api: &HostApi, style: CatalogStyle) -> String {
    match style {
        CatalogStyle::CodexExact => render_codex(api, true),
        CatalogStyle::CodexMinimal => render_codex(api, false),
        CatalogStyle::ProductJavaScript => render_product(api),
        CatalogStyle::Python => unreachable!("Python uses render_pyi"),
    }
}
