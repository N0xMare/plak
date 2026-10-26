//! Canonical row order and tool assembly shared by root and child views.

use super::{tool_facts, tools};
use crate::sessions::projector::{ReadActor, ReadItem, ReadPart};
use crate::sessions::{NoticeScope, ThinkingRow, TranscriptRow};

mod history;
mod tool_rows;
pub(super) use history::canonical_items;
pub(crate) use tool_rows::{projected_tool_row, settle_tool_row};

enum CanonicalPart<'a> {
    Text(&'a str),
    Reasoning(&'a str),
    Hidden,
    Boundary,
}

enum CanonicalRow {
    Row(Box<TranscriptRow>),
    Boundary(usize),
}

fn canonical_rows<'a>(
    actor: ReadActor,
    harness_authored: bool,
    location: (u64, u32),
    sent_at_ms: Option<u64>,
    parts: impl IntoIterator<Item = CanonicalPart<'a>>,
) -> Vec<CanonicalRow> {
    let mut rows = Vec::new();
    let mut text = String::new();
    for (block, part) in parts.into_iter().enumerate() {
        match part {
            CanonicalPart::Text(value) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(value);
            }
            CanonicalPart::Reasoning(value) => {
                flush(&mut rows, &mut text, actor, harness_authored, sent_at_ms);
                if !value.trim().is_empty() {
                    rows.push(CanonicalRow::Row(Box::new(TranscriptRow::Thinking(
                        ThinkingRow {
                            location_key: format!(
                                "{}:{}",
                                location.0,
                                location
                                    .1
                                    .saturating_add(u32::try_from(block).unwrap_or(u32::MAX))
                            ),
                            text: value.to_owned(),
                            elapsed_ms: None,
                        },
                    ))));
                }
            }
            CanonicalPart::Hidden => {}
            CanonicalPart::Boundary => {
                flush(&mut rows, &mut text, actor, harness_authored, sent_at_ms);
                rows.push(CanonicalRow::Boundary(block));
            }
        }
    }
    flush(&mut rows, &mut text, actor, harness_authored, sent_at_ms);
    rows
}

fn flush(
    rows: &mut Vec<CanonicalRow>,
    text: &mut String,
    actor: ReadActor,
    harness_authored: bool,
    sent_at_ms: Option<u64>,
) {
    if text.trim().is_empty() {
        text.clear();
        return;
    }
    let value = std::mem::take(text);
    if let Some(mail) = crate::topology_navigator::mail_row(&value) {
        rows.push(CanonicalRow::Row(Box::new(TranscriptRow::info(
            NoticeScope::Tree,
            mail,
        ))));
        return;
    }
    if harness_authored {
        return;
    }
    match actor {
        ReadActor::User => {
            let (own, mail) = crate::topology_navigator::split_child_mail(&value)
                .unwrap_or_else(|| (value, Vec::new()));
            if !own.trim().is_empty() {
                rows.push(CanonicalRow::Row(Box::new(TranscriptRow::User {
                    text: own,
                    sent_at_ms,
                })));
            }
            rows.extend(mail.into_iter().map(|mail| {
                CanonicalRow::Row(Box::new(TranscriptRow::info(NoticeScope::Tree, mail)))
            }));
        }
        ReadActor::Assistant => {
            rows.push(CanonicalRow::Row(Box::new(TranscriptRow::assistant(value))));
        }
        ReadActor::Other => {}
    }
}

/// What one history item derives, in part order.
pub(crate) enum Derived<'a> {
    /// A prose, mail or reasoning row.
    Row(Box<TranscriptRow>),
    /// A tool call or result block: its row is keyed by call id.
    Call(&'a ReadPart),
}

/// What one history item derives: user and assistant prose, inbound mail as
/// one tree notice (a prompt's child-mail batch as one per child), visible reasoning, and its call blocks in order.
pub(crate) fn item_rows(
    item: &ReadItem,
    ordinal: u64,
    sent_at_ms: Option<u64>,
) -> Vec<Derived<'_>> {
    canonical_rows(
        item.actor,
        item.harness_authored,
        (ordinal, 0),
        sent_at_ms,
        item.parts.iter().map(|part| match part {
            ReadPart::Text(value) | ReadPart::Image(value) => CanonicalPart::Text(value),
            ReadPart::Reasoning(value) => CanonicalPart::Reasoning(value),
            _ => CanonicalPart::Boundary,
        }),
    )
    .into_iter()
    .filter_map(|row| match row {
        CanonicalRow::Row(row) => Some(Derived::Row(row)),
        CanonicalRow::Boundary(index) => match &item.parts[index] {
            ReadPart::Compaction => None,
            part => Some(Derived::Call(part)),
        },
    })
    .collect()
}
