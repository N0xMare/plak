//! The complete face transcript must equal the same journey's durable view.

use super::*;
use plakd_wire_types::v13::{HistoryCursor, HostFrameV13};
use plakous_host_protocol::views::transcript::{
    RowKey, TranscriptRow, TranscriptScope, TranscriptState,
};
use plakous_host_protocol::wire::SnapshotAssembler;

#[derive(Default)]
pub(super) struct FaceTranscript {
    attachment: Option<u64>,
    pages: SnapshotAssembler,
    state: TranscriptState,
    older: BTreeMap<RowKey, TranscriptRow>,
    cursor: Option<HistoryCursor>,
    request: Option<u64>,
    histories: u64,
}

impl FaceTranscript {
    pub(super) fn observe(&mut self, frame: &Value) -> io::Result<()> {
        if frame["frame"] == "history_page" {
            let HostFrameV13::HistoryPage {
                attachment,
                id,
                items,
                last,
                older_cursor,
                ..
            } = serde_json::from_value(frame.clone())?
            else {
                unreachable!("checked history frame tag");
            };
            if Some(attachment) == self.attachment && Some(id) == self.request {
                self.older
                    .extend(items.into_iter().map(|item| (item.key, item.row.0)));
                if last {
                    self.cursor = older_cursor;
                    self.request = None;
                }
            }
            return Ok(());
        }
        let Some(observation) = crate::protocol_13::decode(&frame.to_string()).map_err(invalid)?
        else {
            return Ok(());
        };
        match observation {
            HostFrame::Snapshot(page) if page.scope == TranscriptScope::Root => {
                if self.attachment != Some(page.attachment) {
                    self.attachment = Some(page.attachment);
                    self.pages = SnapshotAssembler::default();
                    self.state = TranscriptState::default();
                }
                if page.page == 0 {
                    self.older.clear();
                    self.cursor = None;
                }
                if let Some(snapshot) = self.pages.push(page).map_err(invalid)? {
                    self.state.replace(snapshot).map_err(invalid)?;
                    self.cursor = frame
                        .get("older_cursor")
                        .cloned()
                        .map(serde_json::from_value)
                        .transpose()?;
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

    fn rows(&self) -> Vec<&TranscriptRow> {
        let mut rows: BTreeMap<_, _> = self.older.iter().collect();
        rows.extend(self.state.items());
        // The session header is synthesized for presentation, not a saved
        // transcript row. A newest page need not include it.
        rows.into_values()
            .filter(|row| !matches!(row, TranscriptRow::SessionHeader { .. }))
            .collect()
    }

    fn next_older(&mut self) -> Option<Value> {
        if self.request.is_some() {
            return None;
        }
        let cursor = self.cursor.take()?;
        let id = u64::MAX - self.histories;
        self.histories += 1;
        self.request = Some(id);
        Some(json!({"frame":"request","id":id,"op":{
            "op":"older_history","attachment":self.attachment?,"cursor":cursor,
        }}))
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
        let expected_rows = expected.values().collect::<Vec<_>>();
        while self.transcript.rows() != expected_rows {
            assert!(
                Instant::now() < until,
                "face transcript differs from durable view: {:?} != {expected:?}",
                self.transcript.state.items()
            );
            if let Some(request) = self.transcript.next_older() {
                self.send(&request)?;
            }
            let frame = self.read().map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "waiting for transcript (actual {:?}, expected {expected_rows:?}): {error}",
                        self.transcript.rows()
                    ),
                )
            })?;
            if frame["frame"] == "response" && frame["id"].as_u64() == self.transcript.request {
                assert_eq!(
                    frame["outcome"]["ok"]["answer"], "history_started",
                    "{frame}"
                );
                continue;
            }
            if matches!(
                frame["frame"].as_str(),
                Some("response" | "welcome" | "refused")
            ) {
                self.controls.push_back(frame);
            }
        }
        assert_eq!(
            self.transcript.rows(),
            expected_rows,
            "missing or duplicated transcript content"
        );
        Ok(())
    }
}

fn durable(fixture: &Fixture, session: &str) -> io::Result<BTreeMap<RowKey, TranscriptRow>> {
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let record =
        plakd::store::ProductStore::inspect_session_read_only(&paths, session).map_err(invalid)?;
    Ok(record
        .transcript
        .items()
        .into_iter()
        .map(|item| (item.key, item.row))
        .collect())
}
