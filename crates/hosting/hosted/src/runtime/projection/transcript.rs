//! One bounded, revisioned presentation publisher for root and agent scopes.

use crate::surface_protocol::ContentEvent;
use std::collections::BTreeMap;

use plakous_host_protocol::ProtocolExtension;
use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::views::transcript::{
    Lane, RowKey, TranscriptItem, TranscriptPatch, TranscriptRow, TranscriptScope, TranscriptState,
};

use crate::surface_protocol::{HostEvent, HostEventSender};

mod live;
mod repair;

fn truncation_key(scope: &TranscriptScope) -> RowKey {
    match scope {
        TranscriptScope::Root => RowKey::new(0, Lane::Runtime, 1),
        TranscriptScope::Agent(_) => RowKey::TRUNCATION,
    }
}

fn apply_bounded(state: &mut TranscriptState, patch: &mut TranscriptPatch) -> bool {
    // Retention removes the oldest body rows, never the session header.
    while state.apply(patch.clone()).is_err() {
        let marker = truncation_key(&patch.scope);
        if !patch.upserts.iter().any(|item| item.key == marker) {
            patch.upserts.push(TranscriptItem {
                key: marker,
                row: TranscriptRow::Truncated,
            });
            patch.upserts.sort_by_key(|item| item.key);
        }
        let Some(key) = state
            .items()
            .keys()
            .find(|key| {
                !patch.deletes.contains(key)
                    && !patch.upserts.iter().any(|item| item.key == **key)
                    && !matches!(
                        state.items().get(key),
                        Some(TranscriptRow::SessionHeader { .. })
                    )
            })
            .copied()
        else {
            return false;
        };
        patch.deletes.push(key);
        patch.deletes.sort_unstable();
    }
    true
}

#[derive(Default)]
pub(in crate::runtime) struct Presentations {
    // One root and at most the one agent this face serves. Revisions remain
    // monotonic across replacements, so a retired serve cannot share a baseline.
    states: BTreeMap<TranscriptScope, TranscriptState>,
    live: BTreeMap<TranscriptScope, live::LiveRows>,
    pub(in crate::runtime) root_busy: bool,
    pub(in crate::runtime) root_terminal_status: bool,
}

impl Presentations {
    pub(in crate::runtime) fn snapshot<P: ProtocolExtension>(
        &mut self,
        mut snapshot: PresentationSnapshot,
        sink: &HostEventSender<P>,
    ) -> bool {
        if let Some(pending) = self
            .states
            .get(&snapshot.scope)
            .filter(|state| state.revision() == 0)
        {
            let committed_calls: std::collections::BTreeSet<_> = snapshot
                .items
                .iter()
                .filter_map(|item| item.row.call_id())
                .collect();
            let mut notices: std::collections::BTreeSet<_> = snapshot
                .items
                .iter()
                .filter_map(|item| match &item.row {
                    TranscriptRow::Notice(notice) => Some(notice.text.as_str()),
                    _ => None,
                })
                .collect();
            let carry: Vec<_> = pending
                .items()
                .iter()
                .filter(|(_, row)| {
                    row.call_id().is_none_or(|id| !committed_calls.contains(id))
                        && !snapshot.items.iter().any(|item| item.row == **row)
                })
                .filter(|(_, row)| match row {
                    TranscriptRow::Notice(notice) => notices.insert(notice.text.as_str()),
                    _ => true,
                })
                .map(|(key, row)| TranscriptItem {
                    key: *key,
                    row: row.clone(),
                })
                .collect();
            snapshot.items.extend(carry);
            snapshot.items.sort_by_key(|item| item.key);
        }
        if let Err(error) = snapshot.validate() {
            debug_assert!(false, "invalid transcript snapshot: {error:?}");
            let _ = sink.send(ContentEvent::Row(TranscriptRow::error(
                crate::sessions::NoticeScope::Host,
                "invalid transcript presentation; rebuilt a bounded view",
            )));
            snapshot = repair::snapshot(snapshot);
        }
        let Some(revision) = sink.next_presentation_revision() else {
            return false;
        };
        snapshot.revision = revision;
        let scope = snapshot.scope.clone();
        let mut state = TranscriptState::default();
        if state.replace(snapshot.clone()).is_err() {
            return false;
        }
        if matches!(scope, TranscriptScope::Agent(_)) {
            self.states
                .retain(|key, _| *key == TranscriptScope::Root || *key == scope);
            self.live
                .retain(|key, _| *key == TranscriptScope::Root || *key == scope);
        }
        self.live
            .entry(scope.clone())
            .or_default()
            .reset(state.items());
        self.states.insert(scope, state);
        sink.send(HostEvent::PresentationSnapshot(snapshot)).is_ok()
    }

    pub(in crate::runtime) fn publish<P: ProtocolExtension>(
        &mut self,
        snapshot: PresentationSnapshot,
        sink: &HostEventSender<P>,
    ) -> bool {
        if snapshot.validate().is_err() {
            return self.snapshot(snapshot, sink);
        }
        if sink.transcript_resnapshot_required_for(&snapshot.scope) {
            return self.snapshot(snapshot, sink);
        }
        let Some(before) = self
            .states
            .get(&snapshot.scope)
            .filter(|state| state.revision() != 0)
        else {
            return self.snapshot(snapshot, sink);
        };
        let Some(revision) = sink.next_presentation_revision() else {
            return false;
        };
        let after = snapshot
            .items
            .into_iter()
            .map(|item| (item.key, item.row))
            .collect();
        let patch = TranscriptPatch::between(
            snapshot.scope.clone(),
            before.revision(),
            revision,
            before.items(),
            &after,
        );
        let Some(state) = self.states.get_mut(&snapshot.scope) else {
            return false;
        };
        if state.apply(patch.clone()).is_err() {
            return false;
        }
        self.live
            .entry(snapshot.scope)
            .or_default()
            .reset(state.items());
        // A refusal is remembered by the mailbox. Keep the current source
        // baseline, so overflow or a consumer's resync gets the complete view.
        let scope = patch.scope.clone();
        if sink.send(HostEvent::TranscriptPatch(patch)).is_ok() {
            true
        } else {
            self.resync(&scope, sink)
        }
    }

    pub(in crate::runtime) fn resync<P: ProtocolExtension>(
        &mut self,
        scope: &TranscriptScope,
        sink: &HostEventSender<P>,
    ) -> bool {
        let Some(state) = self.states.get(scope) else {
            return false;
        };
        let mut live = self.live.remove(scope);
        let mut rows = state.items().clone();
        if let Some(live) = &mut live {
            rows.extend(
                live.drain_pending()
                    .into_iter()
                    .map(|item| (item.key, item.row)),
            );
        }
        let mut snapshot = PresentationSnapshot {
            scope: scope.clone(),
            items: rows
                .into_iter()
                .map(|(key, row)| TranscriptItem { key, row })
                .collect(),
            ..PresentationSnapshot::default()
        };
        if snapshot.validate().is_err() {
            snapshot = repair::snapshot(snapshot);
        }
        let sent = self.snapshot(snapshot, sink);
        if let Some(live) = live {
            let mut live = live;
            if let Some(state) = self.states.get(scope) {
                live.retain(state.items());
            }
            self.live.insert(scope.clone(), live);
        }
        sent
    }

    pub(in crate::runtime) fn clear(&mut self) {
        self.states.clear();
        self.live.clear();
    }

    pub(in crate::runtime) fn close_agent(&mut self) {
        self.states
            .retain(|scope, _| *scope == TranscriptScope::Root);
        self.live.retain(|scope, _| *scope == TranscriptScope::Root);
    }

    pub(in crate::runtime) fn observe<P: ProtocolExtension, E: ProtocolExtension>(
        &mut self,
        scope: TranscriptScope,
        event: &HostEvent<E>,
        sink: &HostEventSender<P>,
    ) -> bool {
        if matches!(scope, TranscriptScope::Agent(_)) && !self.states.contains_key(&scope) {
            self.states.retain(|key, _| *key == TranscriptScope::Root);
            self.live.retain(|key, _| *key == TranscriptScope::Root);
        }
        let state = self.states.entry(scope.clone()).or_insert_with(|| {
            let mut state = TranscriptState::default();
            let _ = state.replace(PresentationSnapshot {
                scope: scope.clone(),
                ..PresentationSnapshot::default()
            });
            state
        });
        let attached = state.revision() != 0;
        // Root streaming starts only after its attachment baseline. Before
        // then retain opening notices, not arbitrary text sent to an unattached
        // face (including root traffic while inspecting a standalone child).
        if !attached && !matches!(event, HostEvent::Row(_)) {
            return true;
        }
        let Some(changes) = self
            .live
            .entry(scope.clone())
            .or_default()
            .observe(event, state.items())
        else {
            if let HostEvent::ToolLine { call_id, name, .. } = event
                && matches!(scope, TranscriptScope::Agent(_))
                && let Some(tool) = self
                    .live
                    .get(&scope)
                    .and_then(|live| live.pending_tool(call_id.as_deref().unwrap_or(name)))
            {
                if sink
                    .send(ContentEvent::TranscriptStream {
                        scope: scope.clone(),
                        delta:
                            plakous_host_protocol::views::transcript::TranscriptStreamDelta::Tool(
                                Box::new(tool.clone()),
                            ),
                    })
                    .is_err()
                {
                    return self.resync(&scope, sink);
                }
            }
            return true;
        };
        if changes.deletes.is_empty()
            && changes
                .upserts
                .iter()
                .all(|item| state.items().get(&item.key) == Some(&item.row))
        {
            return true;
        }
        let revision = if attached {
            let Some(revision) = sink.next_presentation_revision() else {
                return false;
            };
            revision
        } else {
            1
        };
        let mut patch = TranscriptPatch {
            scope: scope.clone(),
            from_revision: state.revision(),
            to_revision: revision,
            upserts: changes.upserts,
            deletes: changes.deletes,
        };
        if !apply_bounded(state, &mut patch) {
            return false;
        }
        if !patch.deletes.is_empty() {
            self.live
                .entry(scope.clone())
                .or_default()
                .retain(state.items());
        }
        if !attached {
            let baseline = PresentationSnapshot {
                scope,
                items: state
                    .items()
                    .iter()
                    .map(|(key, row)| TranscriptItem {
                        key: *key,
                        row: row.clone(),
                    })
                    .collect(),
                ..PresentationSnapshot::default()
            };
            let _ = state.replace(baseline);
            return true;
        }
        if sink.send(HostEvent::TranscriptPatch(patch)).is_err()
            || sink.transcript_resnapshot_required_for(&scope)
        {
            return self.resync(&scope, sink);
        }
        true
    }

    pub(in crate::runtime) fn resync_gaps<P: ProtocolExtension>(
        &mut self,
        sink: &HostEventSender<P>,
    ) {
        let scopes: Vec<_> = self
            .states
            .keys()
            .filter(|scope| sink.transcript_resnapshot_required_for(scope))
            .cloned()
            .collect();
        for scope in scopes {
            self.resync(&scope, sink);
        }
    }
}

#[cfg(test)]
mod tests;
