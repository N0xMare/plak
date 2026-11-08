//! The complete face transcript must equal the same journey's durable view.

use super::*;
use plakous_host_protocol::views::transcript::{Lane, RowKey, TranscriptScope, TranscriptState};
use plakous_host_protocol::wire::SnapshotAssembler;

#[derive(Default)]
pub(super) struct FaceTranscript {
    attachment: Option<u64>,
    pages: SnapshotAssembler,
    state: TranscriptState,
}

impl FaceTranscript {
    pub(super) fn observe(&mut self, frame: &Value) -> io::Result<()> {
        let observation: HostFrame<PlakExtension> = serde_json::from_value(frame.clone())?;
        match observation {
            HostFrame::Snapshot(page) if page.scope == TranscriptScope::Root => {
                if self.attachment != Some(page.attachment) {
                    self.attachment = Some(page.attachment);
                    self.pages = SnapshotAssembler::default();
                    self.state = TranscriptState::default();
                }
                if let Some(snapshot) = self.pages.push(page).map_err(invalid)? {
                    self.state.replace(snapshot).map_err(invalid)?;
                }
            }
            HostFrame::Event {
                attachment,
                observation: plakous_host_protocol::SessionObservation::TranscriptPatch(patch),
                ..
            } if Some(attachment) == self.attachment && patch.scope == TranscriptScope::Root => {
                self.state.apply(patch).map_err(invalid)?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn invalid(error: impl std::fmt::Debug) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{error:?}"))
}

impl Connection {
    pub(super) fn assert_transcript(&mut self, fixture: &Fixture, session: &str) -> io::Result<()> {
        let expected = durable(fixture, session)?;
        let until = Instant::now() + WAIT;
        // Live operator notices have process-local keys until their commit;
        // equality here is complete typed row content, multiplicity and order.
        while self.transcript.state.items().values().collect::<Vec<_>>()
            != expected.values().collect::<Vec<_>>()
        {
            assert!(
                Instant::now() < until,
                "face transcript differs from durable view: {:?} != {expected:?}",
                self.transcript.state.items()
            );
            let frame = self.read()?;
            if matches!(
                frame["frame"].as_str(),
                Some("response" | "welcome" | "refused")
            ) {
                self.controls.push_back(frame);
            }
        }
        assert_eq!(
            self.transcript.state.items().values().collect::<Vec<_>>(),
            expected.values().collect::<Vec<_>>(),
            "missing or duplicated transcript content"
        );
        Ok(())
    }
}

fn durable(
    fixture: &Fixture,
    session: &str,
) -> io::Result<BTreeMap<RowKey, plakous_host_protocol::views::transcript::TranscriptRow>> {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let record =
        plak::store::ProductStore::inspect_session_read_only(&paths, session).map_err(invalid)?;
    let mut expected: BTreeMap<_, _> = record
        .transcript
        .items()
        .into_iter()
        .map(|item| (item.key, item.row))
        .collect();
    expected.insert(
        RowKey::new(0, Lane::Runtime, 0),
        plakous_host_protocol::views::transcript::TranscriptRow::SessionHeader {
            session_id: session.to_owned(),
            carried_from: None,
        },
    );
    Ok(expected)
}

pub(in super::super) fn assert_recorded(fixture: &Fixture, session: &str, entries: &[Value]) {
    let expected = durable(fixture, session).unwrap();
    for connection in [1, 2] {
        let mut face = FaceTranscript::default();
        for entry in entries
            .iter()
            .filter(|entry| entry["connection"] == connection && entry["direction"] == "host")
        {
            face.observe(&entry["frame"]).unwrap();
        }
        assert_eq!(
            face.state.items().values().collect::<Vec<_>>(),
            expected.values().collect::<Vec<_>>(),
            "real-client connection {connection}: missing or duplicated transcript content"
        );
    }
}
