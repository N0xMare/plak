//! Best-effort display minimization, not a credential security boundary.
//!
//! The one text redactor shared by [`crate::SafeDiagnostic`], observation
//! subjects, argument/result previews and hosted session-log previews. It is an
//! unsupported seam re-exported to sibling crates, not a supported API.
//!
//! A host may persist text redacted with these rules and show it again
//! without redacting it (the product transcript, spec 02 §2.6), so a rule
//! change must also force that host's stored rows to be rebuilt.

#![allow(
    clippy::must_use_candidate,
    missing_docs,
    reason = "unsupported sibling seam is documented as one implementation tier"
)]

use std::{borrow::Cow, collections::HashSet, sync::LazyLock};

use regex::Regex;
use serde_json::Value;

/// What a redacted value or span reads as.
pub const MARKER: &str = "[redacted]";
const INDENT_PAD: &str = "                                                                ";
const NAME: &str = r"(?i)^[A-Za-z0-9_.-]*(?:api[_-]?key|token|secret|password|passwd)$";

static CREDENTIAL_NAME: LazyLock<Regex> = LazyLock::new(|| compile(NAME));
static RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"(?s)-----BEGIN (?:[A-Z]+ )?PRIVATE KEY-----.*?-----END (?:[A-Z]+ )?PRIVATE KEY-----", MARKER),
        // A key whose END a cut or a bound left out: every whole key is gone
        // by now, so the rest of the text is the key's.
        (r"(?s)-----BEGIN (?:[A-Z]+ )?PRIVATE KEY-----.*", MARKER),
        (r#"(?im)(\b(?:Authorization|Proxy-Authorization|X-Api-Key|Cookie)[ \t]*:[ \t]*)([^\s"'\[\r\n][^\r\n"']*)"#, "${1}[redacted]"),
        (r"((?i:\bBearer|\bBasic)[ \t]+)[A-Za-z0-9._~+/-]{16,}=*", "${1}[redacted]"),
        (r"\bsk-[A-Za-z0-9_-]{20,}", MARKER),
        (r"\bAKIA[0-9A-Z]{16}\b", MARKER),
        (r"\b(?:gh[posur]_|github_pat_|glpat-|AIza|xox[abprs]-)[A-Za-z0-9_-]{20,}", MARKER),
        (r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+", MARKER),
        (r"(\b[A-Za-z][A-Za-z0-9+.-]*://[^\s/:@]+:)[^\s@/\[]+(@)", "${1}[redacted]${2}"),
        (r#"(?i)([?&](?:token|key|api_key|access_token|password|secret|sig)=)[^\s&#"'\[\]]+"#, "${1}[redacted]"),
    ]
    .into_iter()
    .map(|(pattern, replacement)| (compile(pattern), replacement))
    .collect()
});

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|error| panic!("invalid preview regex {pattern}: {error}"))
}

/// Name, an optional key-closing quote, the separator, then either an escaped
/// quoted body, a plain quoted body, or a bare value. The body is what
/// becomes `[redacted]`; quotes stay with their backslash.
const ASSIGNMENT: &str = r#"(?i)(\b[A-Za-z0-9_.-]*(?:api[_-]?key|token|secret|password|passwd|credential)\b)(["']?)(\s*[:=]\s*)(?:(\\["'])([^\s"'\[\]]{8,})(\\["'])|(["'])([^\s"'\[\]]{8,})(["'])|([^\s"'\[\]]{8,}))"#;

static ASSIGNMENT_RULE: LazyLock<Regex> = LazyLock::new(|| compile(ASSIGNMENT));

pub fn text(input: &str) -> String {
    let mut output = input.to_owned();
    for (rule, replacement) in RULES.iter() {
        if let Cow::Owned(redacted) = rule.replace_all(&output, *replacement) {
            output = redacted;
        }
    }
    if let Cow::Owned(redacted) = ASSIGNMENT_RULE.replace_all(&output, assignment_value) {
        output = redacted;
    }
    output
}

fn assignment_value(caps: &regex::Captures<'_>) -> String {
    let name = &caps[1];
    let pre = &caps[2];
    let separator = &caps[3];
    let quoted = caps
        .get(4)
        .zip(caps.get(6))
        .or(caps.get(7).zip(caps.get(9)));
    match quoted {
        Some((open, close)) => {
            format!(
                "{name}{pre}{separator}{}[redacted]{}",
                open.as_str(),
                close.as_str()
            )
        }
        None => format!("{name}{pre}{separator}[redacted]"),
    }
}

/// Whether an object key is credential-named: [`json`] redacts every string
/// and number leaf under such a key.
#[must_use]
pub fn credential_name(key: &str) -> bool {
    CREDENTIAL_NAME.is_match(key)
}

pub fn json(value: &mut Value) {
    redact(value, false);
}

/// A credential-named key redacts every string and number leaf under it, at
/// any depth. Other string leaves and every key use the text rules; a later
/// key that redacts to an already-kept key is dropped. Booleans and null stay.
fn redact(value: &mut Value, credential: bool) {
    if credential && matches!(value, Value::String(_) | Value::Number(_)) {
        *value = Value::String(MARKER.to_owned());
        return;
    }
    match value {
        Value::Object(object) => {
            let source = std::mem::take(object);
            for (key, mut child) in source {
                let shown = text(&key);
                if object.contains_key(&shown) {
                    continue;
                }
                redact(&mut child, credential || credential_name(&key));
                object.insert(shown, child);
            }
        }
        Value::Array(values) => {
            for child in values {
                redact(child, credential);
            }
        }
        Value::String(value) => *value = text(value),
        _ => {}
    }
}

/// Pretty-prints `value` with the same rules as [`json`], stopping once `limit`
/// bytes have been written. The unread tail is not visited. The bool is true
/// when the value continued past that prefix. A string leaf or key is redacted
/// whole before its encoding is written into the remaining room. Keys are
/// written in source order, so an object whose redacted keys sort differently
/// from their sources lists them in another order than [`json`] does.
#[must_use]
pub fn pretty_bounded(value: &Value, limit: usize) -> (String, bool) {
    let mut writer = Pretty {
        out: String::with_capacity(limit.min(1024 * 1024)),
        limit,
    };
    let clipped = writer.write(value, 0, false);
    (writer.out, clipped)
}

struct Pretty {
    out: String,
    limit: usize,
}

impl Pretty {
    /// True when `data` did not fit whole.
    fn push(&mut self, data: &str) -> bool {
        let room = self.limit.saturating_sub(self.out.len());
        if data.len() <= room {
            self.out.push_str(data);
            return false;
        }
        let mut end = room;
        while end > 0 && !data.is_char_boundary(end) {
            end -= 1;
        }
        self.out.push_str(&data[..end]);
        true
    }

    fn push_indent(&mut self, level: usize) -> bool {
        let mut remaining = level.saturating_mul(2);
        while remaining > 0 {
            let width = remaining.min(INDENT_PAD.len());
            if self.push(&INDENT_PAD[..width]) {
                return true;
            }
            remaining -= width;
        }
        false
    }

    fn write(&mut self, value: &Value, indent: usize, credential: bool) -> bool {
        if self.out.len() >= self.limit {
            return true;
        }
        match value {
            Value::Null => self.push("null"),
            Value::Bool(true) => self.push("true"),
            Value::Bool(false) => self.push("false"),
            Value::Number(_) | Value::String(_) if credential => self.push("\"[redacted]\""),
            Value::Number(number) => self.push(&number.to_string()),
            Value::String(body) => match serde_json::to_string(&text(body)) {
                Ok(encoded) => self.push(&encoded),
                Err(_) => self.push("\"[redacted]\""),
            },
            Value::Array(items) if items.is_empty() => self.push("[]"),
            Value::Object(items) if items.is_empty() => self.push("{}"),
            Value::Array(items) => self.write_array(items, indent, credential),
            Value::Object(items) => self.write_object(items, indent, credential),
        }
    }

    fn write_array(&mut self, items: &[Value], indent: usize, credential: bool) -> bool {
        if self.push("[") {
            return true;
        }
        for (index, item) in items.iter().enumerate() {
            if self.push(if index == 0 { "\n" } else { ",\n" }) || self.push_indent(indent + 1) {
                return true;
            }
            if self.write(item, indent + 1, credential) {
                return true;
            }
        }
        self.push("\n") || self.push_indent(indent) || self.push("]")
    }

    fn write_object(
        &mut self,
        items: &serde_json::Map<String, Value>,
        indent: usize,
        credential: bool,
    ) -> bool {
        if self.push("{") {
            return true;
        }
        let mut shown_keys = HashSet::new();
        for (key, child) in items {
            let shown = text(key);
            if shown_keys.contains(&shown) {
                continue;
            }
            let separator = if shown_keys.is_empty() { "\n" } else { ",\n" };
            if self.push(separator) || self.push_indent(indent + 1) {
                return true;
            }
            let encoded = serde_json::to_string(&shown).unwrap_or_else(|_| "\"\"".to_owned());
            shown_keys.insert(shown);
            if self.push(&encoded) || self.push(": ") {
                return true;
            }
            let child_credential = credential || credential_name(key);
            if self.write(child, indent + 1, child_credential) {
                return true;
            }
        }
        self.push("\n") || self.push_indent(indent) || self.push("}")
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests;
