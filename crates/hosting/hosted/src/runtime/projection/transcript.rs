//! One bounded, revisioned presentation publisher for root and agent scopes.

use std::collections::BTreeMap;

use plakous_host_protocol::ProtocolExtension;
use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::views::transcript::{
    TranscriptItem, TranscriptPatch, TranscriptScope, TranscriptState,
};

use crate::surface_protocol::{HostEvent, HostEventSender};

#[derive(Default)]
pub(in crate::runtime) struct Presentations {
    revision: u64,
    // One root and at most the one agent this face serves. Revisions remain
    // monotonic across replacements, so a retired serve cannot share a baseline.
    states: BTreeMap<TranscriptScope, TranscriptState>,
}

impl Presentations {
    pub(in crate::runtime) fn snapshot<P: ProtocolExtension>(
        &mut self,
        mut snapshot: PresentationSnapshot,
        sink: &HostEventSender<P>,
    ) -> bool {
        let Some(revision) = self.revision.checked_add(1) else {
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
        }
        self.states.insert(scope, state);
        self.revision = revision;
        sink.send(HostEvent::PresentationSnapshot(snapshot)).is_ok()
    }

    pub(in crate::runtime) fn publish<P: ProtocolExtension>(
        &mut self,
        snapshot: PresentationSnapshot,
        sink: &HostEventSender<P>,
    ) -> bool {
        if snapshot.validate().is_err() {
            return false;
        }
        if sink.transcript_resnapshot_required_for(&snapshot.scope) {
            return self.snapshot(snapshot, sink);
        }
        let Some(before) = self.states.get(&snapshot.scope) else {
            return self.snapshot(snapshot, sink);
        };
        let Some(revision) = self.revision.checked_add(1) else {
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
        self.revision = revision;
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
        let snapshot = PresentationSnapshot {
            scope: scope.clone(),
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
        self.snapshot(snapshot, sink)
    }

    pub(in crate::runtime) fn clear(&mut self) {
        self.states.clear();
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
