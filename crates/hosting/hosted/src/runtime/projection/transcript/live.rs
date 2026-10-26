//! One keyed live-row reducer for root and agent transcript scopes.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use plakous_host_protocol::ProtocolExtension;
use plakous_host_protocol::views::transcript::{
    RowKey, ThinkingRow, ToolItem, TranscriptItem, TranscriptRow,
};

use crate::surface_protocol::HostEvent;

#[derive(Default)]
pub(super) struct LiveRows {
    #[cfg(test)]
    pub(super) copied_rows: usize,
    pending: BTreeMap<RowKey, TranscriptRow>,
    assistant: Option<RowKey>,
    calls: HashMap<String, RowKey>,
    thinking: HashMap<String, (RowKey, Instant)>,
}

pub(super) struct LiveChange {
    pub(super) upserts: Vec<TranscriptItem>,
    pub(super) deletes: Vec<RowKey>,
}

impl LiveRows {
    pub(super) fn reset(&mut self, rows: &BTreeMap<RowKey, TranscriptRow>) {
        self.pending.clear();
        self.assistant = None;
        self.thinking.clear();
        self.calls = rows
            .iter()
            .filter_map(|(key, row)| row.call_id().map(|id| (id.to_owned(), *key)))
            .collect();
    }

    fn key(&mut self) -> RowKey {
        crate::sessions::provisional_key()
    }

    pub(super) fn retain(&mut self, rows: &BTreeMap<RowKey, TranscriptRow>) {
        self.calls.retain(|_, key| rows.contains_key(key));
        self.thinking.retain(|_, (key, _)| rows.contains_key(key));
        self.assistant = self.assistant.filter(|key| rows.contains_key(key));
    }

    /// `None` means this observation has no transcript content; an empty
    /// result is a stream boundary, not an append or a revision publication.
    pub(super) fn observe<P: ProtocolExtension>(
        &mut self,
        event: &HostEvent<P>,
        rows: &BTreeMap<RowKey, TranscriptRow>,
    ) -> Option<LiveChange> {
        match event {
            HostEvent::AssistantDelta(delta) => {
                let key = self.assistant.unwrap_or_else(|| self.key());
                self.assistant = Some(key);
                #[cfg(test)]
                if !self.pending.contains_key(&key) && rows.contains_key(&key) {
                    self.copied_rows += 1;
                }
                let row = self.pending.entry(key).or_insert_with(|| {
                    rows.get(&key)
                        .cloned()
                        .unwrap_or_else(|| TranscriptRow::assistant(""))
                });
                if let TranscriptRow::Assistant { text } = row {
                    append_bounded(text, delta, crate::sessions::MAX_TRANSCRIPT_BYTES);
                }
                return None;
            }
            HostEvent::ReasoningDelta {
                location_key,
                text: delta,
            } => {
                self.assistant = None;
                let key = if let Some((key, _)) = self.thinking.get(location_key) {
                    *key
                } else {
                    let key = self.key();
                    self.thinking
                        .insert(location_key.clone(), (key, Instant::now()));
                    key
                };
                #[cfg(test)]
                if !self.pending.contains_key(&key) && rows.contains_key(&key) {
                    self.copied_rows += 1;
                }
                let row = self.pending.entry(key).or_insert_with(|| {
                    rows.get(&key).cloned().unwrap_or_else(|| {
                        TranscriptRow::Thinking(ThinkingRow {
                            location_key: location_key.clone(),
                            text: String::new(),
                            elapsed_ms: None,
                        })
                    })
                });
                if let TranscriptRow::Thinking(thinking) = row {
                    append_reasoning(
                        &mut thinking.text,
                        delta,
                        crate::sessions::MAX_THINKING_ACTIVITY_BYTES,
                    );
                }
                return None;
            }
            _ => {}
        }
        if let HostEvent::ToolLine {
            call_id,
            name,
            state,
            ..
        } = event
        {
            let existing = self.calls.contains_key(call_id.as_deref().unwrap_or(name));
            let (key, row) = self.tool(event, rows)?;
            self.pending.insert(key, crate::sessions::bound_row(row));
            if existing && matches!(state.as_str(), "running" | "waiting") {
                return None;
            }
            return Some(LiveChange {
                upserts: self.drain_pending(),
                deletes: Vec::new(),
            });
        }
        let mut change = self.observe_boundary(event, rows)?;
        for item in change.upserts {
            self.pending.insert(item.key, item.row);
        }
        for key in &change.deletes {
            self.pending.remove(key);
        }
        change.upserts = self.drain_pending();
        Some(change)
    }

    pub(super) fn drain_pending(&mut self) -> Vec<TranscriptItem> {
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(|(key, row)| TranscriptItem {
                key,
                row: crate::sessions::bound_row(row),
            })
            .collect()
    }

    pub(super) fn pending_tool(&self, id: &str) -> Option<&ToolItem> {
        self.calls
            .get(id)
            .and_then(|key| self.pending.get(key))
            .and_then(TranscriptRow::as_tool)
    }

    fn observe_boundary<P: ProtocolExtension>(
        &mut self,
        event: &HostEvent<P>,
        rows: &BTreeMap<RowKey, TranscriptRow>,
    ) -> Option<LiveChange> {
        if let HostEvent::ModelRetry(retry) = event
            && retry.output_discarded
        {
            let mut deletes: Vec<_> = rows
                .iter()
                .rev()
                .take_while(|(key, row)| {
                    key.anchor >= 1_u64 << 63
                        && matches!(
                            row,
                            TranscriptRow::Assistant { .. } | TranscriptRow::Thinking(_)
                        )
                })
                .map(|(key, _)| *key)
                .collect();
            deletes.reverse();
            deletes.extend(self.pending.iter().filter_map(|(key, row)| {
                matches!(
                    row,
                    TranscriptRow::Assistant { .. } | TranscriptRow::Thinking(_)
                )
                .then_some(*key)
            }));
            deletes.sort_unstable();
            deletes.dedup();
            self.assistant = None;
            self.thinking.clear();
            return Some(LiveChange {
                upserts: Vec::new(),
                deletes,
            });
        }
        let (key, row) = match event {
            HostEvent::AssistantDone => {
                self.assistant = None;
                return Some(LiveChange {
                    upserts: Vec::new(),
                    deletes: Vec::new(),
                });
            }
            HostEvent::UserMessage { text, sent_at_ms } => {
                self.assistant = None;
                (
                    self.key(),
                    TranscriptRow::User {
                        text: text.clone(),
                        sent_at_ms: *sent_at_ms,
                    },
                )
            }
            HostEvent::Row(row) => {
                let key = row
                    .call_id()
                    .and_then(|id| self.calls.get(id).copied())
                    .unwrap_or_else(|| self.key());
                if !rows.contains_key(&key) {
                    self.assistant = None;
                }
                if let Some(id) = row.call_id() {
                    self.calls.insert(id.to_owned(), key);
                }
                (key, row.clone())
            }
            HostEvent::ReasoningSealed { location_key } => {
                let Some((key, started)) = self.thinking.remove(location_key) else {
                    return Some(LiveChange {
                        upserts: Vec::new(),
                        deletes: Vec::new(),
                    });
                };
                let Some(TranscriptRow::Thinking(mut thinking)) =
                    self.pending.get(&key).or_else(|| rows.get(&key)).cloned()
                else {
                    return Some(LiveChange {
                        upserts: Vec::new(),
                        deletes: Vec::new(),
                    });
                };
                thinking.elapsed_ms =
                    Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
                (key, TranscriptRow::Thinking(thinking))
            }
            HostEvent::Status { .. } | HostEvent::SetBusy(false) => {
                return Some(LiveChange {
                    upserts: Vec::new(),
                    deletes: Vec::new(),
                });
            }
            _ => return None,
        };
        Some(LiveChange {
            upserts: vec![TranscriptItem {
                key,
                row: crate::sessions::bound_row(row),
            }],
            deletes: Vec::new(),
        })
    }
    fn tool<P: ProtocolExtension>(
        &mut self,
        event: &HostEvent<P>,
        rows: &BTreeMap<RowKey, TranscriptRow>,
    ) -> Option<(RowKey, TranscriptRow)> {
        let HostEvent::ToolLine {
            call_id,
            parent_call_id,
            cell_id,
            kind,
            name,
            state,
            subject,
            detail,
            started_at_ms,
            finished_at_ms,
            duration_ms,
            wait,
            ..
        } = event
        else {
            return None;
        };
        Some({
            let id = call_id.as_deref().unwrap_or(name);
            let key = self.calls.get(id).copied().unwrap_or_else(|| self.key());
            if !rows.contains_key(&key) {
                self.assistant = None;
            }
            self.calls.insert(id.to_owned(), key);
            let old = self
                .pending
                .get(&key)
                .or_else(|| rows.get(&key))
                .and_then(TranscriptRow::as_tool);
            let started_at_ms = started_at_ms.or_else(|| old.and_then(|tool| tool.started_at_ms));
            (
                key,
                TranscriptRow::Tool(ToolItem {
                    call_id: id.to_owned(),
                    name: name.clone(),
                    exposed_name: None,
                    kind: *kind,
                    state: state.clone(),
                    subject: subject
                        .clone()
                        .or_else(|| old.and_then(|tool| tool.subject.clone())),
                    detail: detail
                        .clone()
                        .or_else(|| old.and_then(|tool| tool.detail.clone())),
                    started_at_ms,
                    finished_at_ms: finished_at_ms.or_else(|| {
                        started_at_ms
                            .zip(*duration_ms)
                            .map(|(started, duration)| started.saturating_add(duration))
                    }),
                    parent_call_id: parent_call_id
                        .clone()
                        .or_else(|| old.and_then(|tool| tool.parent_call_id.clone())),
                    cell_id: cell_id
                        .clone()
                        .or_else(|| old.and_then(|tool| tool.cell_id.clone())),
                    wait: wait
                        .clone()
                        .or_else(|| old.and_then(|tool| tool.wait.clone())),
                }),
            )
        })
    }
}

fn append_bounded(text: &mut String, delta: &str, limit: usize) {
    // Retain a bounded tail without serializing or cloning accumulated prose.
    text.push_str(delta);
    if text.len() > limit {
        let mut start = text.len() - limit;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text.drain(..start);
    }
}

fn append_reasoning(text: &mut String, delta: &str, limit: usize) {
    text.push_str(delta);
    if text.len() > limit {
        let marker = crate::sessions::TRUNCATED_ACTIVITY_MARKER;
        let mut end = limit.saturating_sub(marker.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(marker);
    }
}
