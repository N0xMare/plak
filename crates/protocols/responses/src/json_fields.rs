//! Fail-closed pure JSON field extractors for Responses-family wire objects.
//!
//! These helpers operate only on [`serde_json::Map`] / [`serde_json::Value`].
//! Dialects map [`ResponsesFieldError`] into their stream/decode error types at
//! the boundary; provider labels, lifecycle rules, and resource limits stay in
//! the plugins.

use serde_json::{Map, Value};
use thiserror::Error;

/// Fail-closed JSON field extraction failure (no provider identity).
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ResponsesFieldError {
    /// A required field was absent.
    #[error("missing JSON field `{field}`")]
    Missing {
        /// Wire field name.
        field: &'static str,
    },
    /// A field was present with the wrong JSON type, or dual-field conflict.
    #[error("invalid JSON field `{field}`")]
    Invalid {
        /// Wire field name (or the first conflicting field of a dual set).
        field: &'static str,
    },
}

/// Returns a required top-level field value.
///
/// # Errors
///
/// Returns [`ResponsesFieldError::Missing`] when `field` is absent.
pub fn required_value<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a Value, ResponsesFieldError> {
    object
        .get(field)
        .ok_or(ResponsesFieldError::Missing { field })
}

/// Returns a required object-typed field as a map.
///
/// # Errors
///
/// Missing → [`ResponsesFieldError::Missing`]; non-object →
/// [`ResponsesFieldError::Invalid`].
pub fn required_object<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a Map<String, Value>, ResponsesFieldError> {
    required_value(object, field)?
        .as_object()
        .ok_or(ResponsesFieldError::Invalid { field })
}

/// Returns a required string-typed field.
///
/// # Errors
///
/// Missing → [`ResponsesFieldError::Missing`]; non-string →
/// [`ResponsesFieldError::Invalid`].
pub fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, ResponsesFieldError> {
    required_value(object, field)?
        .as_str()
        .ok_or(ResponsesFieldError::Invalid { field })
}

/// Returns an optional string field.
///
/// Absent or JSON `null` yields `Ok(None)`. A non-string non-null value is
/// invalid.
///
/// # Errors
///
/// Returns [`ResponsesFieldError::Invalid`] when the field is present and not a
/// string (and not null).
pub fn optional_string<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<Option<&'a str>, ResponsesFieldError> {
    object
        .get(field)
        .filter(|value| !value.is_null())
        .map(|value| value.as_str().ok_or(ResponsesFieldError::Invalid { field }))
        .transpose()
}

/// Returns a required unsigned 32-bit integer field (JSON number).
///
/// # Errors
///
/// Missing → [`ResponsesFieldError::Missing`]; non-u64-compatible number or
/// overflow of `u32` → [`ResponsesFieldError::Invalid`].
pub fn required_u32(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<u32, ResponsesFieldError> {
    let value = required_u64(object, field)?;
    u32::try_from(value).map_err(|_| ResponsesFieldError::Invalid { field })
}

/// Returns a required unsigned 64-bit integer field (JSON number).
///
/// # Errors
///
/// Missing → [`ResponsesFieldError::Missing`]; non-u64 JSON number →
/// [`ResponsesFieldError::Invalid`].
pub fn required_u64(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<u64, ResponsesFieldError> {
    required_value(object, field)?
        .as_u64()
        .ok_or(ResponsesFieldError::Invalid { field })
}

/// Returns an optional unsigned 64-bit integer field (JSON number).
///
/// Absent or JSON `null` yields `Ok(None)`.
///
/// # Errors
///
/// Returns [`ResponsesFieldError::Invalid`] when the field is present and not a
/// JSON u64 (and not null).
pub fn optional_u64(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<u64>, ResponsesFieldError> {
    object
        .get(field)
        .filter(|value| !value.is_null())
        .map(|value| value.as_u64().ok_or(ResponsesFieldError::Invalid { field }))
        .transpose()
}

/// Returns an optional object-typed field as a map.
///
/// Absent or JSON `null` yields `Ok(None)`.
///
/// # Errors
///
/// Returns [`ResponsesFieldError::Invalid`] when the field is present and not an
/// object (and not null).
pub fn optional_object<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<Option<&'a Map<String, Value>>, ResponsesFieldError> {
    object
        .get(field)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_object()
                .ok_or(ResponsesFieldError::Invalid { field })
        })
        .transpose()
}

/// Maximum bytes accepted for one provider correlation label (id, name, …).
pub const MAX_PROVIDER_LABEL_BYTES: usize = 256;

/// Fail-closed provider label grammar: non-empty, bounded, no control chars.
///
/// # Errors
///
/// Returns [`ResponsesFieldError::Invalid`] when the label is empty, longer than
/// [`MAX_PROVIDER_LABEL_BYTES`], or contains a Unicode control character.
pub fn validate_provider_label(
    value: &str,
    field: &'static str,
) -> Result<(), ResponsesFieldError> {
    if value.is_empty()
        || value.len() > MAX_PROVIDER_LABEL_BYTES
        || value.chars().any(char::is_control)
    {
        Err(ResponsesFieldError::Invalid { field })
    } else {
        Ok(())
    }
}

/// Required string field that also passes [`validate_provider_label`].
///
/// # Errors
///
/// Missing/type errors from [`required_string`], or label grammar failure.
pub fn required_provider_label<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, ResponsesFieldError> {
    let value = required_string(object, field)?;
    validate_provider_label(value, field)?;
    Ok(value)
}

/// Optional string field that also passes [`validate_provider_label`] when set.
///
/// # Errors
///
/// Type errors from [`optional_string`], or label grammar failure.
pub fn optional_provider_label<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<Option<&'a str>, ResponsesFieldError> {
    let value = optional_string(object, field)?;
    if let Some(value) = value {
        validate_provider_label(value, field)?;
    }
    Ok(value)
}

/// Extracts an optional integer from the first present field in `fields`.
///
/// Intended for [`crate::SequencePolicy::OptionalMonotonic`] wire keys (for
/// example `sequence_number` then `sequence`). If **more than one** listed
/// field is present, fails closed with [`ResponsesFieldError::Invalid`] on the
/// first listed field that appears (xAI dual-field conflict semantics).
///
/// Dialects that forbid additional aliases (e.g. `OpenRouter` rejecting
/// `sequence` when only `sequence_number` is accepted) should reject those
/// aliases before calling this helper.
///
/// # Errors
///
/// Dual presence among `fields`, or a present field that is not a JSON u64.
pub fn optional_u64_fields(
    object: &Map<String, Value>,
    fields: &[&'static str],
) -> Result<Option<u64>, ResponsesFieldError> {
    let mut found: Option<(&'static str, &Value)> = None;
    for &field in fields {
        if let Some(value) = object.get(field) {
            if let Some((first, _)) = found {
                return Err(ResponsesFieldError::Invalid { field: first });
            }
            found = Some((field, value));
        }
    }
    match found {
        None => Ok(None),
        Some((field, value)) => value
            .as_u64()
            .ok_or(ResponsesFieldError::Invalid { field })
            .map(Some),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: &Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map.clone(),
            _ => Map::new(),
        }
    }

    #[test]
    fn required_value_missing_and_present() {
        let fixture = json!({"a": 1});
        let map = object(&fixture);
        assert_eq!(
            required_value(&map, "b"),
            Err(ResponsesFieldError::Missing { field: "b" })
        );
        assert_eq!(
            required_value(&map, "a").ok().and_then(Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn required_string_and_object_type_checks() {
        let fixture = json!({"s": "ok", "n": 1, "o": {"k": true}});
        let map = object(&fixture);
        assert_eq!(required_string(&map, "s"), Ok("ok"));
        assert_eq!(
            required_string(&map, "n"),
            Err(ResponsesFieldError::Invalid { field: "n" })
        );
        assert_eq!(
            required_string(&map, "missing"),
            Err(ResponsesFieldError::Missing { field: "missing" })
        );
        assert!(required_object(&map, "o").is_ok());
        assert_eq!(
            required_object(&map, "s"),
            Err(ResponsesFieldError::Invalid { field: "s" })
        );
    }

    #[test]
    fn optional_string_null_absent_and_wrong_type() {
        let fixture = json!({"a": "x", "n": null, "bad": 3});
        let map = object(&fixture);
        assert_eq!(optional_string(&map, "a"), Ok(Some("x")));
        assert_eq!(optional_string(&map, "n"), Ok(None));
        assert_eq!(optional_string(&map, "absent"), Ok(None));
        assert_eq!(
            optional_string(&map, "bad"),
            Err(ResponsesFieldError::Invalid { field: "bad" })
        );
    }

    #[test]
    fn required_u32_and_u64() {
        let fixture = json!({"i": 7u64, "big": u64::from(u32::MAX) + 1, "s": "no"});
        let map = object(&fixture);
        assert_eq!(required_u32(&map, "i"), Ok(7));
        assert_eq!(required_u64(&map, "i"), Ok(7));
        assert_eq!(
            required_u32(&map, "big"),
            Err(ResponsesFieldError::Invalid { field: "big" })
        );
        assert_eq!(required_u64(&map, "big"), Ok(u64::from(u32::MAX) + 1));
        assert_eq!(
            required_u64(&map, "s"),
            Err(ResponsesFieldError::Invalid { field: "s" })
        );
        assert_eq!(
            required_u32(&map, "missing"),
            Err(ResponsesFieldError::Missing { field: "missing" })
        );
    }

    #[test]
    fn optional_u64_fields_try_order_and_dual_conflict() {
        let fields = &["sequence_number", "sequence"];
        let only_primary = object(&json!({"sequence_number": 3}));
        assert_eq!(optional_u64_fields(&only_primary, fields), Ok(Some(3)));

        let only_alias = object(&json!({"sequence": 9}));
        assert_eq!(optional_u64_fields(&only_alias, fields), Ok(Some(9)));

        let neither = object(&json!({"type": "response.created"}));
        assert_eq!(optional_u64_fields(&neither, fields), Ok(None));

        let both = object(&json!({"sequence_number": 1, "sequence": 1}));
        assert_eq!(
            optional_u64_fields(&both, fields),
            Err(ResponsesFieldError::Invalid {
                field: "sequence_number"
            })
        );

        let bad_type = object(&json!({"sequence_number": "nope"}));
        assert_eq!(
            optional_u64_fields(&bad_type, fields),
            Err(ResponsesFieldError::Invalid {
                field: "sequence_number"
            })
        );
    }

    #[test]
    fn optional_u64_fields_single_list_ignores_unlisted_keys() {
        // OpenRouter-style: only sequence_number is listed; bare `sequence` is a
        // dialect pre-check, not dual-field conflict among listed keys.
        let fields = &["sequence_number"];
        let alias_only = object(&json!({"sequence": 1}));
        assert_eq!(optional_u64_fields(&alias_only, fields), Ok(None));
        let primary = object(&json!({"sequence_number": 2, "sequence": 9}));
        assert_eq!(optional_u64_fields(&primary, fields), Ok(Some(2)));
    }
}
