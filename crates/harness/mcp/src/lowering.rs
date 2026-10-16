//! Lowering MCP input schemas onto the portable V2 tool schema.
//!
//! Only meaning-preserving or tightening rewrites apply; the server stays the
//! validator of record. The rules, in order of application per node:
//!
//! | Input | Lowered |
//! | --- | --- |
//! | `$schema`, `$id`, `$comment`, `default`, `examples`, `format`, `readOnly`, `writeOnly`, `deprecated`, `pattern` | dropped |
//! | local `$ref` into `#/$defs` or `#/definitions` | inlined; sibling `title`/`description` win |
//! | `anyOf`/`oneOf` of exactly `[X, {"type":"null"}]`, `type: [X, "null"]` | `X`, and the property leaves `required` |
//! | object without `additionalProperties: false` (absent, `true`, `{}`) | closed |
//! | object without `properties` or `required` | `{}` / `[]` |
//! | node without `type` but with `properties` or a one-type `enum` | that type |
//!
//! Refused: recursive or non-local `$ref`, `additionalProperties` with a
//! schema, `allOf`, other unions, multi-type `type`, `const`, tuple `items`,
//! any other keyword, and anything outside the V2 limits.

use std::fmt;

use plakous_model::PortableToolSchemaV2;
use serde_json::{Map, Value};

const DRAFT_2020_12_URI: &str = "https://json-schema.org/draft/2020-12/schema";

const DROPPED: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "default",
    "examples",
    "format",
    "readOnly",
    "writeOnly",
    "deprecated",
    "pattern",
];

const KEPT: &[&str] = &[
    "title",
    "description",
    "enum",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
];

/// Why an MCP input schema has no portable lowering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaRefusal {
    reason: String,
}

impl SchemaRefusal {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// Returns the refused construct, e.g. `allOf at /properties/a`.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for SchemaRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "input schema is not lowerable: {}", self.reason)
    }
}

impl std::error::Error for SchemaRefusal {}

/// Lowers one MCP `inputSchema` onto a validated [`PortableToolSchemaV2`].
///
/// # Errors
///
/// Returns the first refused construct, or the V2 limit the lowered schema
/// exceeds.
pub fn lower_input_schema(
    schema: &Map<String, Value>,
) -> Result<PortableToolSchemaV2, SchemaRefusal> {
    let definitions = match (schema.get("$defs"), schema.get("definitions")) {
        (Some(Value::Object(defs)), _) => Some(("#/$defs/", defs)),
        (None, Some(Value::Object(defs))) => Some(("#/definitions/", defs)),
        (None, None) => None,
        _ => return Err(SchemaRefusal::new("definitions are not an object")),
    };
    let mut root = schema.clone();
    root.remove("$defs");
    root.remove("definitions");
    let mut lowering = Lowering {
        definitions,
        expanding: Vec::new(),
    };
    let lowered = lowering.node(&Value::Object(root), "")?;
    if lowered.nullable {
        return Err(SchemaRefusal::new("the root schema is nullable"));
    }
    let mut document = lowered.schema;
    if document.get("type").and_then(Value::as_str) != Some("object") {
        return Err(SchemaRefusal::new("the root schema is not an object"));
    }
    document.insert("$schema".into(), Value::String(DRAFT_2020_12_URI.into()));
    PortableToolSchemaV2::new(Value::Object(document)).map_err(|error| {
        SchemaRefusal::new(format!("outside the portable schema limits ({error})"))
    })
}

struct Lowered {
    schema: Map<String, Value>,
    nullable: bool,
}

struct Lowering<'a> {
    definitions: Option<(&'static str, &'a Map<String, Value>)>,
    expanding: Vec<String>,
}

impl Lowering<'_> {
    fn node(&mut self, node: &Value, path: &str) -> Result<Lowered, SchemaRefusal> {
        let Value::Object(object) = node else {
            return Err(SchemaRefusal::new(format!(
                "non-object schema at {}",
                at(path)
            )));
        };
        if let Some(reference) = object.get("$ref") {
            return self.reference(object, reference, path);
        }
        for union in ["anyOf", "oneOf"] {
            if let Some(branches) = object.get(union) {
                return self.nullable_union(object, union, branches, path);
            }
        }
        self.plain(object, path)
    }

    fn reference(
        &mut self,
        object: &Map<String, Value>,
        reference: &Value,
        path: &str,
    ) -> Result<Lowered, SchemaRefusal> {
        let reference = reference
            .as_str()
            .ok_or_else(|| SchemaRefusal::new(format!("$ref at {} is not a string", at(path))))?;
        let target = self
            .definitions
            .and_then(|(prefix, definitions)| {
                reference
                    .strip_prefix(prefix)
                    .and_then(|name| definitions.get(name).map(|target| (name, target)))
            })
            .ok_or_else(|| {
                SchemaRefusal::new(format!(
                    "$ref {reference} at {} is not a local definition",
                    at(path)
                ))
            })?;
        let (name, target) = target;
        if self.expanding.iter().any(|expanding| expanding == name) {
            return Err(SchemaRefusal::new(format!("recursive $ref {reference}")));
        }
        self.expanding.push(name.to_owned());
        let lowered = self.node(target, path);
        self.expanding.pop();
        let mut lowered = lowered?;
        merge_annotations(object, &["$ref"], &mut lowered.schema, path)?;
        Ok(lowered)
    }

    fn nullable_union(
        &mut self,
        object: &Map<String, Value>,
        union: &str,
        branches: &Value,
        path: &str,
    ) -> Result<Lowered, SchemaRefusal> {
        let refused = || SchemaRefusal::new(format!("{union} at {}", at(path)));
        let Some([first, second]) = branches.as_array().map(Vec::as_slice) else {
            return Err(refused());
        };
        let value = match (is_null_schema(first), is_null_schema(second)) {
            (false, true) => first,
            (true, false) => second,
            _ => return Err(refused()),
        };
        let mut lowered = self.node(value, path)?;
        merge_annotations(object, &[union], &mut lowered.schema, path)?;
        lowered.nullable = true;
        Ok(lowered)
    }

    fn plain(&mut self, object: &Map<String, Value>, path: &str) -> Result<Lowered, SchemaRefusal> {
        let mut lowered = Map::new();
        let mut nullable = false;
        for (key, value) in object {
            match key.as_str() {
                key if DROPPED.contains(&key) => {}
                key if KEPT.contains(&key) => {
                    lowered.insert(key.to_owned(), value.clone());
                }
                "type" => {
                    let (kind, null) = lowered_type(value)
                        .ok_or_else(|| SchemaRefusal::new(format!("type at {}", at(path))))?;
                    nullable = null;
                    lowered.insert("type".into(), Value::String(kind));
                }
                "properties" | "required" | "additionalProperties" | "items" => {}
                other => {
                    return Err(SchemaRefusal::new(format!("{other} at {}", at(path))));
                }
            }
        }
        if nullable && let Some(Value::Array(values)) = lowered.get_mut("enum") {
            values.retain(|value| !value.is_null());
        }
        if !lowered.contains_key("type") {
            let kind = inferred_type(object)
                .ok_or_else(|| SchemaRefusal::new(format!("untyped schema at {}", at(path))))?;
            lowered.insert("type".into(), Value::String(kind.into()));
        }
        match lowered.get("type").and_then(Value::as_str) {
            Some("object") => self.object(object, &mut lowered, path)?,
            Some("array") => {
                let items = match object.get("items") {
                    Some(items @ Value::Object(_)) => items,
                    Some(_) => {
                        return Err(SchemaRefusal::new(format!("tuple items at {}", at(path))));
                    }
                    None => {
                        return Err(SchemaRefusal::new(format!(
                            "array without items at {}",
                            at(path)
                        )));
                    }
                };
                let items = self.node(items, &pointer(path, &["items"]))?;
                if items.nullable {
                    return Err(SchemaRefusal::new(format!(
                        "nullable items at {}",
                        at(path)
                    )));
                }
                lowered.insert("items".into(), Value::Object(items.schema));
            }
            _ => {
                for key in ["properties", "required", "additionalProperties", "items"] {
                    if object.contains_key(key) {
                        return Err(SchemaRefusal::new(format!("{key} at {}", at(path))));
                    }
                }
            }
        }
        Ok(Lowered {
            schema: lowered,
            nullable,
        })
    }

    fn object(
        &mut self,
        object: &Map<String, Value>,
        lowered: &mut Map<String, Value>,
        path: &str,
    ) -> Result<(), SchemaRefusal> {
        match object.get("additionalProperties") {
            None | Some(Value::Bool(_)) => {}
            Some(Value::Object(schema)) if schema.is_empty() => {}
            Some(_) => {
                return Err(SchemaRefusal::new(format!(
                    "additionalProperties with a schema at {}",
                    at(path)
                )));
            }
        }
        let properties = match object.get("properties") {
            None => Map::new(),
            Some(Value::Object(properties)) => properties.clone(),
            Some(_) => return Err(SchemaRefusal::new(format!("properties at {}", at(path)))),
        };
        let mut required = match object.get("required") {
            None => Vec::new(),
            Some(Value::Array(names)) => names
                .iter()
                .map(|name| name.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| SchemaRefusal::new(format!("required at {}", at(path))))?,
            Some(_) => return Err(SchemaRefusal::new(format!("required at {}", at(path)))),
        };
        let mut lowered_properties = Map::new();
        for (name, property) in &properties {
            let child = self.node(property, &pointer(path, &["properties", name]))?;
            if child.nullable {
                required.retain(|required| required != name);
            }
            lowered_properties.insert(name.clone(), Value::Object(child.schema));
        }
        lowered.insert("properties".into(), Value::Object(lowered_properties));
        lowered.insert(
            "required".into(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
        lowered.insert("additionalProperties".into(), Value::Bool(false));
        Ok(())
    }
}

/// Copies `title` and `description` beside a `$ref` or union onto the
/// lowered target; any other sibling keyword is refused.
fn merge_annotations(
    object: &Map<String, Value>,
    consumed: &[&str],
    lowered: &mut Map<String, Value>,
    path: &str,
) -> Result<(), SchemaRefusal> {
    for (key, value) in object {
        match key.as_str() {
            key if consumed.contains(&key) || DROPPED.contains(&key) => {}
            "title" | "description" => {
                lowered.insert(key.clone(), value.clone());
            }
            other => {
                return Err(SchemaRefusal::new(format!(
                    "{other} beside {} at {}",
                    consumed[0],
                    at(path)
                )));
            }
        }
    }
    Ok(())
}

fn is_null_schema(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.get("type").and_then(Value::as_str) == Some("null")
            && object
                .keys()
                .all(|key| key == "type" || key == "title" || key == "description")
    })
}

/// A single type, or a pair of one type and `"null"`.
fn lowered_type(value: &Value) -> Option<(String, bool)> {
    match value {
        Value::String(kind) if kind != "null" => Some((kind.clone(), false)),
        Value::Array(kinds) => {
            let kinds = kinds
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()?;
            match kinds.as_slice() {
                [kind] if *kind != "null" => Some(((*kind).to_owned(), false)),
                [kind, "null"] | ["null", kind] if *kind != "null" => {
                    Some(((*kind).to_owned(), true))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn inferred_type(object: &Map<String, Value>) -> Option<&'static str> {
    if object.contains_key("properties") {
        return Some("object");
    }
    let values = object.get("enum")?.as_array()?;
    let kind = |value: &Value| match value {
        Value::String(_) => Some("string"),
        Value::Bool(_) => Some("boolean"),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some("integer"),
        Value::Number(_) => Some("number"),
        _ => None,
    };
    let first = kind(values.first()?)?;
    values
        .iter()
        .all(|value| kind(value) == Some(first))
        .then_some(first)
}

/// Extends a JSON pointer into the schema by `segments`.
fn pointer(path: &str, segments: &[&str]) -> String {
    let mut extended = path.to_owned();
    for segment in segments {
        extended.push('/');
        extended.push_str(segment);
    }
    extended
}

fn at(path: &str) -> &str {
    if path.is_empty() { "/" } else { path }
}

#[cfg(test)]
mod tests;
