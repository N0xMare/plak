//! Encodes baselines for the background jobs and the driver's bounded fallback.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use plakous_host_protocol::views::presentation::PresentationSnapshot;
use plakous_host_protocol::wire::{
    AttachmentId, HostFrame, MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_PAGES, encode_line, page_snapshot,
};
use tokio::sync::{Notify, Semaphore};

use super::SendError;
use crate::surface_protocol::PlakExtension;
use crate::test_hooks::soak_stamp::Stamp;

const BYTES: usize = MAX_SNAPSHOT_BYTES + 64 * 1024; // page envelopes
const FRAMES: usize = MAX_SNAPSHOT_PAGES as usize;

// An idle connection may wait for a slot, but a closed connection must not
// leave queued work behind. A running encoder holds its slot until it ends.
pub(super) static ENCODERS: Semaphore = Semaphore::const_new(4);

#[derive(Clone, Copy)]
pub(super) struct Projection {
    pub(super) generation: u64,
    pub(super) token: uuid::Uuid,
}

#[derive(Default)]
pub(super) struct Pages {
    pub(super) lines: VecDeque<Vec<u8>>,
    pub(super) bytes: usize,
}

pub(super) struct Job {
    result: Arc<Mutex<Option<Result<Pages, SendError>>>>,
    abort: tokio::task::AbortHandle,
}

impl Job {
    pub(super) fn start(
        attachment: AttachmentId,
        snapshot: PresentationSnapshot,
        encoders: &'static Semaphore,
        ready: Arc<Notify>,
        stamp: Stamp,
        projection: Option<Projection>,
    ) -> Self {
        let result = Arc::new(Mutex::new(None));
        let output = Arc::clone(&result);
        let task = tokio::spawn(async move {
            let pages = match encoders.acquire().await {
                Ok(permit) => tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    encode(attachment, snapshot, stamp, projection)
                })
                .await
                .unwrap_or(Err(SendError::Disconnected)),
                Err(_) => Err(SendError::Disconnected),
            };
            *output.lock().unwrap_or_else(PoisonError::into_inner) = Some(pages);
            ready.notify_one();
        });
        Self {
            result,
            abort: task.abort_handle(),
        }
    }

    pub(super) fn ready(&self) -> bool {
        self.result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    pub(super) fn take(&self) -> Result<Pages, SendError> {
        self.result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or(Err(SendError::Disconnected))
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

pub(super) fn encode(
    attachment: AttachmentId,
    snapshot: PresentationSnapshot,
    stamp: Stamp,
    projection: Option<Projection>,
) -> Result<Pages, SendError> {
    let mut output = Pages::default();
    let pages = page_snapshot(snapshot, attachment).map_err(|_| SendError::Snapshot)?;
    if pages.len() > FRAMES {
        return Err(SendError::Snapshot);
    }
    for page in pages {
        let line = match projection {
            Some(Projection { generation, token }) => {
                use plakd_wire_types::v13::{Baseline, HostFrameV13, PageItem, RetainedRevision};
                let live_rows = super::super::protocol_13::live_lengths(&page.items);
                encode_line(&HostFrameV13::Snapshot(Baseline {
                    attachment,
                    scope: page.scope,
                    revision: RetainedRevision {
                        host_generation: generation,
                        revision: page.revision,
                        token: token.to_string(),
                    },
                    part: page.page,
                    last: page.last,
                    live_rows,
                    items: page
                        .items
                        .into_iter()
                        .map(|item| PageItem {
                            key: item.key,
                            row: plakous_host_protocol::wire::FramedTranscriptRow(item.row),
                            truncated_bytes: None,
                        })
                        .collect(),
                    has_older: false,
                    older_rows: 0,
                    older_bytes: 0,
                    older_cursor: None,
                }))
            }
            None => encode_line(&HostFrame::<PlakExtension>::Snapshot(page)),
        };
        let mut line = line.map_err(|_| SendError::Snapshot)?.into_bytes();
        stamp.append(&mut line);
        output.bytes += line.len();
        if output.bytes > BYTES {
            return Err(SendError::Snapshot);
        }
        output.lines.push_back(line);
    }
    Ok(output)
}
