//! Depth validation shared by retained preludes and terminal writes.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{MAX_CELL_VALUE_DEPTH, json_depth};

/// A retained JSON value exceeds the cell's container-depth limit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredValueDepthError {
    /// The refused storage key.
    pub key: String,
    /// The value's measured container depth.
    pub depth: usize,
}

impl std::fmt::Display for StoredValueDepthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stored value {:?} nests {} containers; limit is {MAX_CELL_VALUE_DEPTH}",
            self.key, self.depth
        )
    }
}

impl std::error::Error for StoredValueDepthError {}

/// Checks each retained value independently of the surrounding frame/map.
///
/// # Errors
/// Returns the lexicographically first offending key and its depth when a
/// value is deeper than [`MAX_CELL_VALUE_DEPTH`].
pub fn validate_stored_values<S: std::hash::BuildHasher>(
    stored: &HashMap<String, Value, S>,
) -> Result<(), StoredValueDepthError> {
    let mut keys: Vec<_> = stored.keys().collect();
    keys.sort_unstable();
    for key in keys {
        let value = &stored[key];
        let depth = json_depth(value);
        if depth > MAX_CELL_VALUE_DEPTH {
            return Err(StoredValueDepthError {
                key: key.clone(),
                depth,
            });
        }
    }
    Ok(())
}

pub(crate) fn serialize<S: serde::Serializer>(
    stored: &HashMap<String, Value>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    validate_stored_values(stored).map_err(serde::ser::Error::custom)?;
    stored.serialize(serializer)
}

pub(crate) fn deserialize<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<HashMap<String, Value>, D::Error> {
    let stored = HashMap::deserialize(deserializer)?;
    validate_stored_values(&stored).map_err(serde::de::Error::custom)?;
    Ok(stored)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "protocol boundary assertions")]
mod tests {
    use super::*;
    use crate::{CellOutcome, CellPrelude};

    #[test]
    fn depth_refusal_names_the_lexicographically_first_offending_key() {
        let value = (0..41).fold(Value::Null, |value, _| serde_json::json!([value]));
        for keys in [["z", "a"], ["a", "z"]] {
            let stored = keys
                .into_iter()
                .map(|key| (key.into(), value.clone()))
                .collect::<HashMap<_, _>>();
            assert_eq!(
                validate_stored_values(&stored),
                Err(StoredValueDepthError {
                    key: "a".into(),
                    depth: 41
                })
            );
        }
    }

    #[test]
    fn prelude_and_both_terminals_bound_each_stored_value_in_both_directions() {
        for depth in [40, 41, 50] {
            let value = (0..depth).fold(Value::Null, |value, _| serde_json::json!([value]));
            let stored = HashMap::from([("deep".into(), value.clone())]);
            let validated = validate_stored_values(&stored);
            if depth == 40 {
                assert_eq!(validated, Ok(()));
            } else {
                assert_eq!(
                    validated,
                    Err(StoredValueDepthError {
                        key: "deep".into(),
                        depth
                    })
                );
            }
            let prelude = CellPrelude::Quickjs {
                tools: Vec::new(),
                stored: stored.clone(),
            };
            assert_eq!(serde_json::to_vec(&prelude).is_ok(), depth == 40);
            let wire =
                serde_json::json!({"engine": "quickjs", "tools": [], "stored": {"deep": value}});
            assert_eq!(
                serde_json::from_value::<CellPrelude>(wire).is_ok(),
                depth == 40
            );
            for outcome in [
                CellOutcome::Completed {
                    value: None,
                    stored: stored.clone(),
                },
                CellOutcome::Failed {
                    message: "failed".into(),
                    stored: stored.clone(),
                    generation_lost: false,
                },
            ] {
                assert_eq!(serde_json::to_vec(&outcome).is_ok(), depth == 40);
            }
            for outcome in ["completed", "failed"] {
                let wire = serde_json::json!({"outcome": outcome, "value": null, "message": "failed", "generation_lost": false, "stored": {"deep": value}});
                assert_eq!(
                    serde_json::from_value::<CellOutcome>(wire).is_ok(),
                    depth == 40
                );
            }
        }
    }
}
