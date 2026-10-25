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
            self.assistant = None;
            self.thinking.clear();
            return Some(LiveChange {
                upserts: Vec::new(),
                deletes,
            });
        }
        let (key, row) = match event {
            HostEvent::AssistantDelta(delta) => {
                let key = if let Some(key) = self.assistant {
                    key
                } else {
                    let key = self.key();
                    self.assistant = Some(key);
                    key
                };
                let mut text = match rows.get(&key) {
                    Some(TranscriptRow::Assistant { text }) => text.clone(),
                    _ => String::new(),
                };
                text.push_str(delta);
                (key, TranscriptRow::assistant(text))
            }
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
            HostEvent::ToolLine { .. } => self.tool(event, rows)?,
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
                let mut text = match rows.get(&key) {
                    Some(TranscriptRow::Thinking(thinking)) => thinking.text.clone(),
                    _ => String::new(),
                };
                text.push_str(delta);
                (
                    key,
                    TranscriptRow::Thinking(ThinkingRow {
                        location_key: location_key.clone(),
                        text,
                        elapsed_ms: None,
                    }),
                )
            }
            HostEvent::ReasoningSealed { location_key } => {
                let Some((key, started)) = self.thinking.remove(location_key) else {
                    return Some(LiveChange {
                        upserts: Vec::new(),
                        deletes: Vec::new(),
                    });
                };
                let Some(TranscriptRow::Thinking(mut thinking)) = rows.get(&key).cloned() else {
                    return Some(LiveChange {
                        upserts: Vec::new(),
                        deletes: Vec::new(),
                    });
                };
                thinking.elapsed_ms =
                    Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
                (key, TranscriptRow::Thinking(thinking))
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
            let old = rows.get(&key).and_then(TranscriptRow::as_tool);
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
                    started_at_ms: started_at_ms
                        .or_else(|| old.and_then(|tool| tool.started_at_ms)),
                    finished_at_ms: *finished_at_ms,
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
