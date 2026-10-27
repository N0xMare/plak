//! A cancellation-safe parked-cell mailbox with bounded output and a reserved terminal.

use std::sync::Arc;

use super::{CellCompletion, CellResponse, CollectedOutput, OutputItem, ParkedCells, output_bytes};
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
    truncated: bool,
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
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            if state.truncated {
                break;
            }
            if state.output.append(item, false).is_err() {
                // Keep the bounded prefix and explicitly mark omitted output.
                // The terminal has its own slot even when the output is full.
                let warning = OutputItem::Text {
                    text: "Warning: cell output omitted: collected output limit".into(),
                };
                while state.output.append(warning.clone(), false).is_err() {
                    if let Some(item) = state.output.items.pop() {
                        state.output.bytes -= output_bytes(&item);
                    }
                }
                state.truncated = true;
            }
        }
        drop(state);
        self.changed.notify_one();
    }

    fn take(&self, cell: u64) -> Option<CellCompletion> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.yielded && state.terminal.is_none() {
            return None;
        }
        let items = std::mem::take(&mut state.output).items;
        state.yielded = false;
        state.truncated = false;
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
mod tests {
    use super::super::{MAX_COLLECTED_OUTPUT_BYTES, MAX_COLLECTED_OUTPUT_ITEMS};
    use super::*;

    #[test]
    fn coalesced_output_is_bounded_without_discarding_the_terminal() {
        for item in [
            OutputItem::Text {
                text: "x".repeat(MAX_COLLECTED_OUTPUT_BYTES / 2),
            },
            OutputItem::Text { text: "x".into() },
        ] {
            let entry = ParkedCell::default();
            let count = if output_bytes(&item) > 1 {
                3
            } else {
                MAX_COLLECTED_OUTPUT_ITEMS + 1
            };
            for _ in 0..count {
                entry.publish(Ok(CellResponse::Yielded {
                    cell: 1,
                    items: vec![item.clone()],
                }));
            }
            entry.publish(Ok(CellResponse::Result {
                cell: 1,
                items: Vec::new(),
                error: None,
            }));
            let Some(Ok(CellResponse::Result {
                items, error: None, ..
            })) = entry.take(1)
            else {
                panic!("the terminal survives output saturation")
            };
            assert!(items.len() <= MAX_COLLECTED_OUTPUT_ITEMS);
            assert!(items.iter().map(output_bytes).sum::<usize>() <= MAX_COLLECTED_OUTPUT_BYTES);
            assert!(
                matches!(items.last(), Some(OutputItem::Text { text }) if text.contains("output omitted"))
            );
            assert!(entry.take(1).is_none());
        }
    }

    #[test]
    fn an_abandoned_resume_is_not_issued_twice() {
        let entry = ParkedCell::default();
        assert!(!entry.begin_resume(), "the initial observation is running");
        entry.publish(Ok(CellResponse::Yielded {
            cell: 1,
            items: Vec::new(),
        }));
        assert!(!entry.begin_resume(), "observe the buffered yield first");
        assert!(matches!(
            entry.take(1),
            Some(Ok(CellResponse::Yielded { .. }))
        ));
        assert!(entry.begin_resume());
        assert!(
            !entry.begin_resume(),
            "a cancelled waiter still owns its resume"
        );
    }
}
