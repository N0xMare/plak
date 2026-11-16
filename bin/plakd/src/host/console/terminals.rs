//! Principal-bound terminal requests and bounded, independently credited output.

mod mapping;
mod requests;
mod stream;
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "bounded operator stream fixtures")]
mod tests;

use super::{
    Console, HostPrincipal, HostRequest, HostRequestContext, HostResponse, PoisonError, refused,
};
use crate::terminal_registry::{
    TerminalError, TerminalHandle, TerminalRegistry, controller::TerminalController,
};
use plakd_wire_types::{PlakExtension, console as dto};
use plakous::runner::protocol::operator::{
    OperatorId as RunnerId, OperatorRefusal as RunnerRefusal, TerminalCursor as RunnerCursor,
    TerminalDimensions as RunnerDimensions, TerminalEvent as RunnerEvent, TerminalKey as RunnerKey,
    TerminalProcessIdentity as RunnerProcessIdentity, TerminalStatus as RunnerStatus,
};
use plakous_host_protocol::wire::{HostFrame, RequestRefusal};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Notify, mpsc, watch};

const WINDOW: u64 = 64 * 1024;
const CHUNK: usize = 16 * 1024;

type Owner = (HostPrincipal, u64, u64);

#[derive(Default)]
pub(super) struct State {
    next: u64,
    last: u64,
    streams: BTreeMap<u64, Stream>,
    notifications: BTreeMap<Owner, Arc<Notify>>,
    output: BTreeMap<HostPrincipal, Budget>,
}

struct Stream {
    owner: Owner,
    terminal: dto::TerminalRef,
    commands: mpsc::Sender<Command>,
    resize: watch::Sender<Option<RunnerDimensions>>,
    events: VecDeque<dto::TerminalEvent>,
    seq: u64,
    sent_output: u64,
    next_input: u64,
    acknowledged_input: u64,
    input_tokens: f64,
    input_updated: Instant,
    ended: bool,
    ready: bool,
    output: Budget,
}

struct Budget {
    tokens: f64,
    updated: Instant,
}

impl Budget {
    fn new(limit: u32) -> Self {
        Self {
            tokens: f64::from(limit),
            updated: Instant::now(),
        }
    }
    fn available(&self, now: Instant, limit: u32) -> f64 {
        (self.tokens + now.saturating_duration_since(self.updated).as_secs_f64() * f64::from(limit))
            .min(f64::from(limit))
    }
    fn charge(&mut self, now: Instant, limit: u32, bytes: u32) {
        self.tokens = self.available(now, limit) - f64::from(bytes);
        self.updated = now;
    }
}

fn output_bytes(event: &dto::TerminalEvent) -> Option<u32> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    match event {
        dto::TerminalEvent::Data { bytes_b64, .. } if bytes_b64.len() <= CHUNK.div_ceil(3) * 4 => {
            let bytes = STANDARD.decode(bytes_b64).ok()?;
            (!bytes.is_empty() && bytes.len() <= CHUNK && STANDARD.encode(&bytes) == *bytes_b64)
                .then(|| u32::try_from(bytes.len()).ok())
                .flatten()
        }
        dto::TerminalEvent::Data { .. } => None,
        _ => Some(0),
    }
}

enum Command {
    Input {
        offset: u64,
        bytes: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<Result<(), dto::ReadRefusal>>,
    },
    Credit(u64),
}

fn owner(context: &HostRequestContext) -> Owner {
    (
        context.principal.clone(),
        context.connection_id,
        context.host_generation,
    )
}

pub(in crate::host) fn is_request(request: &HostRequest) -> bool {
    matches!(
        request,
        HostRequest::TerminalOpen { .. }
            | HostRequest::TerminalList { .. }
            | HostRequest::TerminalAttach { .. }
            | HostRequest::TerminalResize { .. }
            | HostRequest::TerminalClose { .. }
            | HostRequest::TerminalDetach { .. }
            | HostRequest::TerminalInput { .. }
            | HostRequest::TerminalCreditBatch { .. }
            | HostRequest::TerminalReleaseSession { .. }
    )
}

pub(super) fn limits() -> dto::TerminalLimits {
    dto::TerminalLimits {
        chunk_bytes: CHUNK as u64,
        credit_bytes: WINDOW,
        unsent_input_bytes: WINDOW,
        ring_bytes: 1024 * 1024,
        terminals_per_session: 2,
        terminals_per_principal: 4,
        terminals_host: 16,
        input_bytes_per_second: WINDOW,
        output_bytes_per_second: 256 * 1024,
        principal_output_bytes_per_second: 512 * 1024,
        scheduler_messages_per_second: 12,
        scheduler_burst: 12,
        scheduler_actions: 32,
        input_coalesce_min_ms: 50,
        input_coalesce_max_ms: 100,
        idle_ms: 900_000,
        detach_ms: 300_000,
        namespace_ms: 90_000,
    }
}

impl Console {
    pub(super) fn terminal_capability(
        &self,
        principal: &HostPrincipal,
    ) -> Result<(), RequestRefusal> {
        if matches!(principal, HostPrincipal::Socket(_)) || self.switches.web_terminal {
            Ok(())
        } else {
            Err(RequestRefusal::CapabilityDisabled {
                capability: plakous_host_protocol::wire::ConsoleCapability::Terminal,
            })
        }
    }

    pub(super) fn terminal_disconnect(&self, context: &HostRequestContext) {
        let mut state = self
            .terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let owner = owner(context);
        state.streams.retain(|_, stream| stream.owner != owner);
        state.notifications.remove(&owner);
    }

    pub(in crate::host) fn terminal_notify(
        &self,
        context: &HostRequestContext,
    ) -> Option<Arc<Notify>> {
        self.terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .notifications
            .get(&owner(context))
            .cloned()
    }

    pub(in crate::host) fn terminal_output(
        &self,
        context: &HostRequestContext,
    ) -> Option<HostFrame<PlakExtension>> {
        let mut state = self
            .terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let owner = owner(context);
        let eligible = |(_, stream): &(&u64, &Stream)| {
            let now = Instant::now();
            stream.owner == owner
                && stream.ready
                && stream
                    .events
                    .front()
                    .and_then(output_bytes)
                    .is_some_and(|bytes| {
                        stream.output.available(now, 256 * 1024) >= f64::from(bytes)
                            && state.output.get(&context.principal).is_some_and(|budget| {
                                budget.available(now, 512 * 1024) >= f64::from(bytes)
                            })
                    })
        };
        let id = state
            .streams
            .range((state.last.saturating_add(1))..)
            .find(eligible)
            .or_else(|| state.streams.iter().find(eligible))
            .map(|(id, _)| *id)?;
        state.last = id;
        let now = Instant::now();
        let bytes = state
            .streams
            .get(&id)?
            .events
            .front()
            .and_then(output_bytes)?;
        state
            .output
            .get_mut(&context.principal)?
            .charge(now, 512 * 1024, bytes);
        let stream = state.streams.get_mut(&id)?;
        stream.output.charge(now, 256 * 1024, bytes);
        let event = stream.events.pop_front()?;
        stream.sent_output += u64::from(bytes);
        let frame = HostFrame::TerminalStream {
            terminal: stream.terminal.id.clone(),
            stream: id,
            seq: stream.seq,
            event,
        };
        stream.seq += 1;
        if state
            .streams
            .values()
            .any(|s| s.owner == owner && !s.events.is_empty())
        {
            state.notifications.get(&owner)?.notify_one();
        }
        Some(frame)
    }

    pub(in crate::host) fn terminal_response_sent(
        &self,
        context: &HostRequestContext,
        response: &HostResponse,
    ) {
        if let HostResponse::TerminalAttached { stream, .. } = response {
            let mut state = self
                .terminals
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(stream) = state
                .streams
                .get_mut(stream)
                .filter(|s| s.owner == owner(context))
            {
                stream.ready = true;
            }
            if let Some(notify) = state.notifications.get(&owner(context)) {
                notify.notify_one();
            }
        }
    }
}
