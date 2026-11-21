//! Prepares a baseline without occupying the request driver or the writer.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use plakous_host_protocol::views::presentation::PresentationSnapshot;
use plakous_host_protocol::wire::{
    AttachmentId, HostFrame, MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_PAGES, encode_line, page_snapshot,
};
use tokio::sync::{Notify, Semaphore};

use super::SendError;
use crate::surface_protocol::PlakExtension;

const BYTES: usize = MAX_SNAPSHOT_BYTES + 64 * 1024; // page envelopes
const FRAMES: usize = MAX_SNAPSHOT_PAGES as usize;

// An idle connection may wait for a slot, but a closed connection must not
// leave queued work behind. A running encoder holds its slot until it ends.
pub(super) static ENCODERS: Semaphore = Semaphore::const_new(4);

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
        charge: impl FnOnce(&Pages) -> Result<(), SendError> + Send + 'static,
    ) -> Self {
        let result = Arc::new(Mutex::new(None));
        let output = Arc::clone(&result);
        let task = tokio::spawn(async move {
            let pages = match encoders.acquire().await {
                Ok(permit) => tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    encode(attachment, snapshot)
                })
                .await
                .unwrap_or(Err(SendError::Disconnected)),
                Err(_) => Err(SendError::Disconnected),
            }
            .and_then(|pages| {
                charge(&pages)?;
                Ok(pages)
            });
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

fn encode(attachment: AttachmentId, snapshot: PresentationSnapshot) -> Result<Pages, SendError> {
    let mut output = Pages::default();
    let pages = page_snapshot(snapshot, attachment).map_err(|_| SendError::Disconnected)?;
    if pages.len() > FRAMES {
        return Err(SendError::Full);
    }
    for page in pages {
        let line = encode_line(&HostFrame::<PlakExtension>::Snapshot(page))
            .map_err(|_| SendError::Disconnected)?
            .into_bytes();
        output.bytes += line.len();
        if output.bytes > BYTES {
            return Err(SendError::Full);
        }
        output.lines.push_back(line);
    }
    Ok(output)
}
