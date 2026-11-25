use serde_json::Value;

const SCHEMA: &str =
    include_str!("../../../../../bin/plakd/tests/fixtures/protocol-13-wire-schema.json");

pub(super) fn validate(direction: &str, fixture: &str) -> Result<(), Box<dyn std::error::Error>> {
    let document: Value = serde_json::from_str(SCHEMA)?;
    let schema = &document[direction];
    for (index, line) in fixture.lines().enumerate() {
        let frame: Value = serde_json::from_str(line)?;
        assert!(
            matches(&document, schema, &frame),
            "{direction} line {} does not match the pinned schema: {line}",
            index + 1
        );
    }
    Ok(())
}

// Evaluate the assertion vocabulary used by the pinned schema. Unknown
// keywords fail the test; format and other annotations are not assertions.
fn matches(root: &Value, schema: &Value, value: &Value) -> bool {
    if let Some(flag) = schema.as_bool() {
        return flag;
    }
    let Some(rules) = schema.as_object() else {
        return false;
    };
    rules.iter().all(|(key, rule)| match key.as_str() {
        "$ref" => rule
            .as_str()
            .and_then(|reference| reference.strip_prefix('#'))
            .and_then(|pointer| root.pointer(pointer))
            .is_some_and(|target| matches(root, target, value)),
        "type" => rule.as_array().map_or_else(
            || has_type(value, rule),
            |types| types.iter().any(|kind| has_type(value, kind)),
        ),
        "anyOf" => branches(rule).any(|branch| matches(root, branch, value)),
        "oneOf" => {
            branches(rule)
                .filter(|branch| matches(root, branch, value))
                .count()
                == 1
        }
        "not" => !matches(root, rule, value),
        "const" => value == rule,
        "enum" => branches(rule).any(|entry| entry == value),
        "properties" => rule.as_object().is_some_and(|fields| {
            fields.iter().all(|(key, spec)| {
                value
                    .get(key)
                    .is_none_or(|entry| matches(root, spec, entry))
            })
        }),
        "required" => {
            !value.is_object()
                || branches(rule)
                    .all(|key| key.as_str().is_some_and(|key| value.get(key).is_some()))
        }
        "additionalProperties" => value.as_object().is_none_or(|fields| {
            fields.iter().all(|(key, entry)| {
                schema["properties"].get(key).is_some() || matches(root, rule, entry)
            })
        }),
        "prefixItems" => value.as_array().is_none_or(|items| {
            branches(rule)
                .zip(items)
                .all(|(spec, entry)| matches(root, spec, entry))
        }),
        "items" => value.as_array().is_none_or(|items| {
            let prefix = schema["prefixItems"].as_array().map_or(0, Vec::len);
            items
                .iter()
                .skip(prefix)
                .all(|entry| matches(root, rule, entry))
        }),
        "minItems" => value
            .as_array()
            .is_none_or(|items| at_limit(items.len(), rule, true)),
        "maxItems" => value
            .as_array()
            .is_none_or(|items| at_limit(items.len(), rule, false)),
        "minLength" => value
            .as_str()
            .is_none_or(|text| at_limit(text.chars().count(), rule, true)),
        "maxLength" => value
            .as_str()
            .is_none_or(|text| at_limit(text.chars().count(), rule, false)),
        "minimum" => value
            .as_f64()
            .is_none_or(|n| rule.as_f64().is_some_and(|min| n >= min)),
        "maximum" => value
            .as_f64()
            .is_none_or(|n| rule.as_f64().is_some_and(|max| n <= max)),
        "pattern" => value.as_str().is_none_or(|text| {
            rule.as_str()
                .and_then(|pattern| regex::Regex::new(pattern).ok())
                .is_some_and(|pattern| pattern.is_match(text))
        }),
        "description" | "default" | "format" | "writeOnly" => true,
        other => panic!("unhandled schema keyword: {other}"),
    })
}

fn branches(rule: &Value) -> impl Iterator<Item = &Value> {
    rule.as_array().into_iter().flatten()
}

fn at_limit(length: usize, rule: &Value, min: bool) -> bool {
    rule.as_u64()
        .and_then(|bound| usize::try_from(bound).ok())
        .is_some_and(|bound| {
            if min {
                length >= bound
            } else {
                length <= bound
            }
        })
}

fn has_type(value: &Value, kind: &Value) -> bool {
    match kind.as_str() {
        Some("array") => value.is_array(),
        Some("boolean") => value.is_boolean(),
        Some("integer") => value.as_f64().is_some_and(|n| n.fract() == 0.0),
        Some("null") => value.is_null(),
        Some("number") => value.is_number(),
        Some("object") => value.is_object(),
        Some("string") => value.is_string(),
        _ => panic!("unhandled schema type: {kind}"),
    }
}
