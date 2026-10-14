//! A bounded JSON read for history results past the exact bound.

use plakous_tools::internal::preview;
use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use std::fmt;

/// Parses a JSON document as [`super::windowed_leaves`] would copy it: each
/// string leaf and key is cut to its redacted history window as it is
/// visited, so the value never retains more than the window of a string (the
/// parser's scratch buffer holds an escaped string, or one read through the
/// joined-parts reader, whole while it is decoded), and once the window's
/// byte budget is kept the rest of the document is only validated. `None`
/// when the source is not one JSON document.
pub(super) fn windowed_json<'de, R: serde_json::de::Read<'de>>(
    mut deserializer: serde_json::Deserializer<R>,
) -> Option<serde_json::Value> {
    let mut budget = super::HISTORY_WINDOW_BYTES;
    let value = WindowedValue {
        budget: &mut budget,
    }
    .deserialize(&mut deserializer)
    .ok()?;
    deserializer.end().ok()?;
    Some(value)
}

struct WindowedValue<'b> {
    budget: &'b mut usize,
}

impl WindowedValue<'_> {
    fn spend(&mut self, bytes: usize) {
        *self.budget = self.budget.saturating_sub(bytes.max(1));
    }
}

impl<'de> DeserializeSeed<'de> for WindowedValue<'_> {
    type Value = serde_json::Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for WindowedValue<'_> {
    type Value = serde_json::Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(mut self, value: bool) -> Result<Self::Value, E> {
        self.spend(8);
        Ok(serde_json::Value::Bool(value))
    }

    fn visit_i64<E>(mut self, value: i64) -> Result<Self::Value, E> {
        self.spend(8);
        Ok(serde_json::Value::Number(value.into()))
    }

    fn visit_u64<E>(mut self, value: u64) -> Result<Self::Value, E> {
        self.spend(8);
        Ok(serde_json::Value::Number(value.into()))
    }

    fn visit_f64<E>(mut self, value: f64) -> Result<Self::Value, E> {
        self.spend(8);
        Ok(serde_json::Number::from_f64(value)
            .map_or(serde_json::Value::Null, serde_json::Value::Number))
    }

    fn visit_unit<E>(mut self) -> Result<Self::Value, E> {
        self.spend(8);
        Ok(serde_json::Value::Null)
    }

    fn visit_str<E>(mut self, value: &str) -> Result<Self::Value, E> {
        let kept = if *self.budget == 0 {
            String::new()
        } else {
            super::windowed_text(value)
        };
        self.spend(kept.len());
        Ok(serde_json::Value::String(kept))
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut items: A) -> Result<Self::Value, A::Error> {
        self.spend(1);
        let mut kept = Vec::new();
        while *self.budget > 0 {
            let seed = WindowedValue {
                budget: &mut *self.budget,
            };
            let Some(item) = items.next_element_seed(seed)? else {
                return Ok(serde_json::Value::Array(kept));
            };
            kept.push(item);
        }
        while items.next_element_seed(Skip)?.is_some() {}
        Ok(serde_json::Value::Array(kept))
    }

    fn visit_map<A: MapAccess<'de>>(mut self, mut fields: A) -> Result<Self::Value, A::Error> {
        self.spend(1);
        let mut kept = serde_json::Map::new();
        while *self.budget > 0 {
            let Some(key) = fields.next_key_seed(WindowedKey)? else {
                return Ok(serde_json::Value::Object(kept));
            };
            self.spend(key.text.len());
            let value = if key.credential {
                fields.next_value_seed(Skip)?;
                self.spend(preview::MARKER.len());
                serde_json::Value::String(preview::MARKER.to_owned())
            } else {
                let seed = WindowedValue {
                    budget: &mut *self.budget,
                };
                fields.next_value_seed(seed)?
            };
            kept.insert(key.text, value);
        }
        while fields.next_key_seed(Skip)?.is_some() {
            fields.next_value_seed(Skip)?;
        }
        Ok(serde_json::Value::Object(kept))
    }
}

/// An object key cut to its redacted history window.
struct WindowedKey;

/// A key the window cut no longer matches the JSON key rule, so whether it is
/// credential-named is decided on the full key; its value is then the
/// redaction marker. A key within the window is kept whole for that rule.
struct Key {
    text: String,
    credential: bool,
}

impl<'de> DeserializeSeed<'de> for WindowedKey {
    type Value = Key;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl Visitor<'_> for WindowedKey {
    type Value = Key;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an object key")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Key {
            credential: value.len() > super::HISTORY_WINDOW_BYTES
                && preview::credential_name(value),
            text: super::windowed_text(value),
        })
    }
}

/// One JSON value parsed as strictly as the live `Value` parse (`serde_json`'s
/// recursion limit, escape and surrogate checks, number range) and dropped.
/// `IgnoredAny` would only scan it, so history would take the JSON path for a
/// document live reads as text.
struct Skip;

impl<'de> DeserializeSeed<'de> for Skip {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Skip {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<Self::Value, A::Error> {
        while items.next_element_seed(Skip)?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut fields: A) -> Result<Self::Value, A::Error> {
        while fields.next_key_seed(Skip)?.is_some() {
            fields.next_value_seed(Skip)?;
        }
        Ok(())
    }
}
