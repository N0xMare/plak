//! Typed canonical execution values become the catalog's plain-data records.

use plakous_cell::CellValue as V;
use plakous_compose::PythonHarnessSdkDescriptorV1 as Limits;
use plakous_tools::{ExecutionValue, FileChangeAction, ListedEntryKind};
use std::io::Write;

pub(super) fn record(name: &str, fields: impl IntoIterator<Item = (&'static str, V)>) -> V {
    V::Record {
        name: name.into(),
        fields: fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    }
}
pub(super) fn text(value: impl Into<String>) -> V {
    V::Str(value.into())
}
fn int(value: u64) -> V {
    V::from(serde_json::Value::from(value))
}
pub(super) fn omission(what: &str, count: u64) -> V {
    record(
        "Omission",
        [
            ("what", text(what)),
            ("count", int(count)),
            ("bytes", V::Int(0)),
            ("full", V::Null),
        ],
    )
}
pub(super) fn job(id: u64) -> V {
    record("Job", [("id", text(id.to_string()))])
}

pub(super) fn convert(value: ExecutionValue) -> Result<V, String> {
    Ok(match value {
        ExecutionValue::FileRead(v) => file(v),
        ExecutionValue::Listing(v) => record(
            "Listing",
            [
                (
                    "entries",
                    V::List(
                        v.entries
                            .into_iter()
                            .map(|e| {
                                record(
                                    "Entry",
                                    [
                                        ("path", text(e.path)),
                                        (
                                            "kind",
                                            text(match e.kind {
                                                ListedEntryKind::File => "file",
                                                ListedEntryKind::Directory => "dir",
                                                ListedEntryKind::Link => "link",
                                                ListedEntryKind::Other => "other",
                                            }),
                                        ),
                                        ("size", int(e.size)),
                                    ],
                                )
                            })
                            .collect(),
                    ),
                ),
                (
                    "omitted",
                    V::List(omissions("entries", v.entries_cut, v.incomplete)),
                ),
            ],
        ),
        ExecutionValue::SearchHits(v) => {
            let clipped = v.hits.iter().filter(|h| h.truncated).count() as u64;
            let mut omitted = omissions("hits", v.hits_cut, v.incomplete);
            if clipped > 0 {
                omitted.push(omission("clipped_hits", clipped));
            }
            record(
                "Hits",
                [
                    (
                        "hits",
                        V::List(
                            v.hits
                                .into_iter()
                                .map(|h| {
                                    record(
                                        "Hit",
                                        [
                                            ("path", text(h.path)),
                                            ("line", int(h.line)),
                                            ("text", text(h.text)),
                                        ],
                                    )
                                })
                                .collect(),
                        ),
                    ),
                    ("omitted", V::List(omitted)),
                ],
            )
        }
        ExecutionValue::FileChanges(v) => V::List(
            v.changes
                .into_iter()
                .map(|c| {
                    V::Dict(vec![
                        ("path".into(), text(c.path)),
                        (
                            "action".into(),
                            text(match c.action {
                                FileChangeAction::Add => "add",
                                FileChangeAction::Update => "update",
                                FileChangeAction::Delete => "delete",
                            }),
                        ),
                        (
                            "revision".into(),
                            c.revision.map_or(V::Null, |v| text(v.digest().to_string())),
                        ),
                    ])
                })
                .collect(),
        ),
        ExecutionValue::Process(v) => process(v),
        ExecutionValue::Plan(v) => int(v.revision),
        _ => return Err("unsupported SDK execution value".into()),
    })
}

fn file(v: plakous_tools::FileReadValue) -> V {
    let mut omitted = Vec::new();
    for (what, count) in [
        ("lines", v.lines_cut),
        ("clipped_lines", v.clipped_lines),
        ("lossy_utf8", u64::from(v.lossy)),
    ] {
        if count > 0 {
            omitted.push(omission(what, count));
        }
    }
    record(
        "File",
        [
            ("path", text(v.path)),
            ("text", text(v.text)),
            ("first_line", int(v.first_line)),
            ("lines", int(v.lines)),
            ("eof", V::Bool(v.eof)),
            (
                "revision",
                text(
                    v.revision
                        .map(|v| v.digest().to_string())
                        .unwrap_or_default(),
                ),
            ),
            ("image", V::Null),
            ("omitted", V::List(omitted)),
        ],
    )
}
fn omissions(what: &str, count: u64, incomplete: bool) -> Vec<V> {
    let mut rows = Vec::new();
    if count > 0 {
        rows.push(omission(what, count));
    }
    // Incomplete traversal does not invent a count for unseen entries.
    if incomplete {
        rows.push(omission("incomplete_walk", 0));
    }
    rows
}

pub(super) fn process(v: plakous_tools::ProcessValue) -> V {
    let mut omitted = Vec::new();
    if v.truncated || v.omitted_bytes > 0 || v.full_output.is_some() {
        omitted.push(record(
            "Omission",
            [
                (
                    "what",
                    text(if v.truncated {
                        "output_truncated"
                    } else {
                        "output"
                    }),
                ),
                ("count", V::Int(0)),
                ("bytes", int(v.omitted_bytes)),
                (
                    "full",
                    v.full_output
                        .map_or(V::Null, |id| record("Ref", [("id", text(id))])),
                ),
            ],
        ));
    }
    record(
        "ProcResult",
        [
            ("job", job(v.job_id)),
            ("status", text(v.status)),
            (
                "exit_code",
                v.exit_code.map_or(V::Null, |n| V::Int(i64::from(n))),
            ),
            ("output", text(v.output)),
            ("omitted", V::List(omitted)),
        ],
    )
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ValueQuotaError {
    #[error("SDK result encodes to at least {bytes} bytes (maximum {limit})", limit = Limits::RESULT_BYTES)]
    Result { bytes: usize },
    #[error("SDK cell results encode to at least {bytes} bytes (maximum {limit})", limit = Limits::CELL_BYTES)]
    Cell { bytes: usize },
    #[error("SDK result encoding failed")]
    Encoding(#[source] serde_json::Error),
}

#[derive(Default)]
pub(super) struct ValueBudget {
    bytes: usize,
}
impl ValueBudget {
    pub(super) fn admit(&mut self, value: &V) -> Result<(), ValueQuotaError> {
        let limit = Limits::RESULT_BYTES.min(Limits::CELL_BYTES.saturating_sub(self.bytes));
        let bytes = bounded_bytes(value, limit).map_err(ValueQuotaError::Encoding)?;
        if bytes > Limits::RESULT_BYTES {
            return Err(ValueQuotaError::Result { bytes });
        }
        let total = self.bytes.saturating_add(bytes);
        if total > Limits::CELL_BYTES {
            return Err(ValueQuotaError::Cell { bytes: total });
        }
        self.bytes = total;
        Ok(())
    }
}

struct Count {
    bytes: usize,
    limit: usize,
}
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes) {
            self.bytes = self.limit + 1;
            return Err(std::io::Error::other("encoded value exceeds budget"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn result_bytes(value: &V) -> Result<usize, ValueQuotaError> {
    let bytes = bounded_bytes(value, Limits::RESULT_BYTES).map_err(ValueQuotaError::Encoding)?;
    if bytes > Limits::RESULT_BYTES {
        return Err(ValueQuotaError::Result { bytes });
    }
    Ok(bytes)
}

fn bounded_bytes(value: &V, limit: usize) -> Result<usize, serde_json::Error> {
    let mut count = Count { bytes: 0, limit };
    match serde_json::to_writer(&mut count, value) {
        _ if count.bytes > count.limit => Ok(count.bytes),
        Err(error) => Err(error),
        Ok(()) => Ok(count.bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quota_charges_the_encoded_cell_value_and_refusals_do_not_spend_capacity() {
        let mut budget = ValueBudget::default();
        let escaped = V::Str("\\".repeat(600_000));
        assert!(matches!(
            budget.admit(&escaped),
            Err(ValueQuotaError::Result { .. })
        ));
        assert_eq!(budget.bytes, 0);
        let value = V::Str("x".repeat(1_000_000));
        for _ in 0..16 {
            assert!(budget.admit(&value).is_ok());
        }
        let used = budget.bytes;
        assert!(matches!(
            budget.admit(&value),
            Err(ValueQuotaError::Cell { .. })
        ));
        assert_eq!(budget.bytes, used);
    }
    #[test]
    fn omissions_preserve_counts_and_do_not_invent_unseen_matches() {
        let values = omissions("hits", 7, true);
        assert_eq!(
            values,
            vec![omission("hits", 7), omission("incomplete_walk", 0)]
        );
    }
}
