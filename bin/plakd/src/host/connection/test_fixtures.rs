//! A deterministic paced baseline shared by the transport tests.

use super::{Connection, Outbox, PresentationSnapshot};
use crate::surface_protocol::PlakExtension;
use plakous_host_protocol::SessionObservation;
use plakous_host_protocol::views::transcript::{
    Lane, RowKey, TranscriptItem, TranscriptPatch, TranscriptRow, TranscriptScope,
};
use plakous_hosted::host_service::wire::{Face, HostPrincipal, SessionHost};

pub(crate) fn history() -> Vec<SessionObservation<PlakExtension>> {
    let row = |key, bytes| TranscriptItem {
        key: RowKey::new(key, Lane::Runtime, 0),
        row: TranscriptRow::assistant("x".repeat(bytes)),
    };
    let snapshot = PresentationSnapshot {
        items: (1..=3).map(|key| row(key, 600 * 1024)).collect(),
        ..PresentationSnapshot::empty(1)
    };
    let mut observations = vec![SessionObservation::PresentationSnapshot(snapshot)];
    for revision in 1..=2 {
        observations.push(SessionObservation::TranscriptPatch(TranscriptPatch {
            scope: TranscriptScope::Root,
            from_revision: revision,
            to_revision: revision + 1,
            upserts: vec![row(revision + 3, 32)],
            deletes: Vec::new(),
        }));
    }
    observations
}

pub(crate) fn connection(
    host: &std::sync::Arc<SessionHost<PlakExtension>>,
    outbox: &Outbox,
    principal: HostPrincipal,
) -> Connection {
    let snapshots = outbox.clone();
    host.connect(outbox.clone(), Face::Remote, principal, || false)
        // Synchronous preparation avoids a paused test clock racing a
        // blocking encoder. The real ordered page cursor is still used.
        .with_snapshot_output(move |attachment, snapshot, _| {
            snapshots.snapshot_inline(attachment, snapshot, None)
        })
}

pub(crate) async fn wait_for_snapshot(outbox: &Outbox, lines: usize) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(1));
    for _ in 0..100 {
        let stats = outbox.close_stats();
        if stats.in_flight == "snapshot" && stats.queued_lines == lines {
            return;
        }
        interval.tick().await;
    }
    panic!("snapshot did not block with the expected number of queued lines");
}
