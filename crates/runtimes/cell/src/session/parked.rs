//! A cancellation-safe parked-cell mailbox with bounded output and a reserved terminal.

use std::sync::Arc;

use super::{CellCompletion, CellResponse, CollectedOutput, ParkedCells};
use tokio::sync::Notify;

pub(super) struct ParkedCell {
    state: std::sync::Mutex<Mailbox>,
    changed: Notify,
}

#[derive(Default)]
struct Mailbox {
    output: CollectedOutput,
    yielded: bool,
    terminal: Option<Result<Option<String>, String>>,
    resume_in_flight: bool,
}

impl Default for ParkedCell {
    fn default() -> Self {
        Self {
            state: std::sync::Mutex::new(Mailbox {
                resume_in_flight: true,
                ..Mailbox::default()
            }),
            changed: Notify::new(),
        }
    }
}

impl ParkedCell {
    pub(super) fn begin_resume(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.yielded || state.terminal.is_some() || state.resume_in_flight {
            return false;
        }
        state.resume_in_flight = true;
        true
    }

    pub(super) fn publish(&self, response: CellCompletion) {
        self.publish_collected(response, false);
    }

    pub(super) fn publish_collected(&self, response: CellCompletion, truncated: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.terminal.is_some() {
            return;
        }
        let items = match response {
            Ok(CellResponse::Yielded { items, .. }) => {
                state.yielded = true;
                state.resume_in_flight = false;
                items
            }
            Ok(CellResponse::Result { items, error, .. }) => {
                state.terminal = Some(Ok(error));
                items
            }
            Err(loss) => {
                state.terminal = Some(Err(loss));
                Vec::new()
            }
        };
        for item in items {
            state.output.append_truncated(item);
        }
        // A truncated producer batch stays truncated while later yields coalesce.
        state.output.truncated |= truncated;
        drop(state);
        self.changed.notify_one();
    }

    pub(super) fn resuming(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.resume_in_flight && !state.yielded && state.terminal.is_none()
    }

    pub(super) fn take(&self, cell: u64) -> Option<CellCompletion> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.yielded && state.terminal.is_none() {
            return None;
        }
        let items = std::mem::take(&mut state.output).items;
        state.yielded = false;
        Some(match state.terminal.take() {
            Some(Ok(error)) => Ok(CellResponse::Result { cell, items, error }),
            Some(Err(loss)) => Err(loss),
            None => Ok(CellResponse::Yielded { cell, items }),
        })
    }
}

pub(super) async fn observe_parked(
    streams: ParkedCells,
    cell: u64,
    entry: Arc<ParkedCell>,
) -> CellCompletion {
    loop {
        let changed = entry.changed.notified();
        // Acquire the table before consuming anything: cancellation at an
        // await must not remove the only copy of output or the terminal.
        let mut streams = streams.lock().await;
        if let Some(response) = entry.take(cell) {
            if !matches!(response, Ok(CellResponse::Yielded { .. })) {
                streams.remove(&cell);
            }
            return response;
        }
        drop(streams);
        changed.await;
    }
}

#[cfg(test)]
mod tests;
