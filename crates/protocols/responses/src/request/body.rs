//! Top-level Responses request body helpers (flags, instructions).

use serde_json::{Map, Value};

/// Joins instruction texts with `"\n\n"`.
///
/// Returns [`None`] when the iterator is empty. When at least one text is
/// present, returns `Some` even if the joined string is empty (dialects that
/// omit empty instructions should filter after join).
#[must_use]
pub fn join_instruction_texts<'a, I>(texts: I) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut iter = texts.into_iter();
    let first = iter.next()?;
    let mut capacity = first.len();
    let mut rest = Vec::new();
    for text in iter {
        capacity = capacity.saturating_add(2).saturating_add(text.len());
        rest.push(text);
    }
    let mut joined = String::with_capacity(capacity);
    joined.push_str(first);
    for text in rest {
        joined.push_str("\n\n");
        joined.push_str(text);
    }
    Some(joined)
}

/// Inserts top-level `stream` and `store` boolean flags into a body map.
///
/// Dialects choose the boolean values (e.g. OpenAI `store` from storage mode;
/// xAI / OpenRouter foundation keep `store: false`).
pub fn insert_stream_store(body: &mut Map<String, Value>, stream: bool, store: bool) {
    body.insert("stream".to_owned(), Value::Bool(stream));
    body.insert("store".to_owned(), Value::Bool(store));
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

    #[test]
    fn join_instructions_empty_single_and_multi() {
        assert_eq!(join_instruction_texts(std::iter::empty::<&str>()), None);
        assert_eq!(join_instruction_texts(["only"]), Some("only".to_owned()));
        assert_eq!(
            join_instruction_texts(["a", "b", "c"]),
            Some("a\n\nb\n\nc".to_owned())
        );
        // Present empty segment still yields Some (caller may filter).
        assert_eq!(join_instruction_texts([""]), Some(String::new()));
    }

    #[test]
    fn stream_store_flags() {
        let mut body = Map::new();
        insert_stream_store(&mut body, true, false);
        assert_eq!(body.get("stream"), Some(&json!(true)));
        assert_eq!(body.get("store"), Some(&json!(false)));
    }
}
