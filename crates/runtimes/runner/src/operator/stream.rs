use plakous_runner_protocol::{StreamEvent, StreamFrame};

use super::{
    CHUNK_BYTES, Controller, Duration, HashMap, Instant, OperatorId, OperatorRefusal,
    OperatorTerminalService, RING_BYTES, RateBudget, STANDARD, Table, Terminal, TerminalCursor,
    TerminalEvent, TerminalStatus, WINDOW_BYTES, lock,
};
use base64::Engine as _;

impl OperatorTerminalService {
    /// Applies a terminal stream frame. None means it is not this connection's
    /// operator stream. Corruption detaches only that controller, never the shell.
    #[must_use]
    pub fn receive(
        &self,
        host: &str,
        generation: &str,
        connection: u64,
        frame: StreamFrame,
    ) -> Option<Vec<StreamFrame>> {
        self.tick();
        let now = (self.inner.clock)();
        let mut table = lock(&self.inner.table);
        for (key, ns) in &mut table.namespaces {
            if key.host != host || key.generation != generation {
                continue;
            }
            for terminal in ns.terminals.values_mut() {
                if !terminal
                    .controller
                    .as_ref()
                    .is_some_and(|c| c.connection == connection && c.stream == frame.stream)
                {
                    continue;
                }
                let mut controller = terminal.controller.take()?;
                if frame.seq != controller.inbound_seq {
                    terminal.detached = Some(now);
                    return Some(vec![controller.frame(StreamEvent::Close {
                        reason: Some("sequence_gap".into()),
                    })]);
                }
                controller.inbound_seq += 1;
                let mut responses = Vec::new();
                let keep = Self::receive_event(
                    ns.reconciled == connection && !ns.closed,
                    terminal,
                    &mut controller,
                    frame.event,
                    now,
                    &mut responses,
                );
                if keep {
                    terminal.controller = Some(controller);
                } else {
                    terminal.detached = Some(now);
                }
                return Some(responses);
            }
        }
        None
    }

    fn receive_event(
        reconciled: bool,
        terminal: &mut Terminal,
        controller: &mut Controller,
        event: StreamEvent,
        now: Instant,
        responses: &mut Vec<StreamFrame>,
    ) -> bool {
        match event {
            StreamEvent::Close { .. } => return false,
            StreamEvent::Credit { bytes } => {
                if bytes == 0 || controller.credit.saturating_add(bytes) > WINDOW_BYTES {
                    responses.push(controller.frame(StreamEvent::Close {
                        reason: Some("credit_overrun".into()),
                    }));
                    return false;
                }
                controller.credit += bytes;
            }
            StreamEvent::Data { payload } => {
                let result = if reconciled {
                    terminal.input(payload, now)
                } else {
                    Err(OperatorRefusal::ReconcileRequired)
                };
                let event = match result {
                    Ok(next) => TerminalEvent::InputAccepted { next },
                    Err(refusal) => TerminalEvent::Refused { refusal },
                };
                responses.push(controller.event(event));
            }
        }
        true
    }

    /// Produces one fair round of output within existing credits. The embedding
    /// transport calls at most 16 rounds/awake second: 256 KiB/s per terminal,
    /// 512 KiB/s per principal. No-credit readers do not block PTY draining.
    #[must_use]
    pub fn output_round(&self, host: &str, generation: &str, connection: u64) -> Vec<StreamFrame> {
        self.tick();
        let mut table = lock(&self.inner.table);
        let now = (self.inner.clock)();
        let mut frames = Vec::new();
        let mut counts: HashMap<OperatorId, u64> = HashMap::new();
        for (key, ns) in &table.namespaces {
            if key.host == host {
                *counts.entry(key.namespace.principal.clone()).or_default() +=
                    ns.terminals
                        .values()
                        .filter(|t| t.controller.is_some())
                        .count() as u64;
            }
        }
        let Table {
            namespaces,
            output_budgets,
            ..
        } = &mut *table;
        for (key, ns) in namespaces {
            if key.host != host || key.generation != generation {
                continue;
            }
            let budget = output_budgets
                .entry((key.host.clone(), key.namespace.principal.clone()))
                .or_insert_with(|| RateBudget::new(now, 2 * CHUNK_BYTES as u64));
            let share = (2 * CHUNK_BYTES as u64
                / counts
                    .get(&key.namespace.principal)
                    .copied()
                    .unwrap_or(1)
                    .max(1))
            .min(CHUNK_BYTES as u64);
            let period = Duration::from_millis(64);
            // Rotate start by stream id each round to avoid fixed-order starvation.
            let mut terminals: Vec<_> = ns.terminals.values_mut().collect();
            terminals.sort_by_key(|t| {
                t.controller
                    .as_ref()
                    .map_or(0, |c| c.cursor / CHUNK_BYTES as u64)
            });
            for terminal in terminals {
                let Some(mut controller) = terminal.controller.take() else {
                    continue;
                };
                if controller.connection == connection {
                    let allowed = budget
                        .available(now, period, 2 * CHUNK_BYTES as u64)
                        .min(controller.output_rate.available(now, period, share));
                    let mut room = usize::try_from(allowed).unwrap_or(0);
                    terminal.output(
                        &self.inner.instance,
                        &mut controller,
                        &mut room,
                        &mut frames,
                    );
                    let sent = allowed - room as u64;
                    budget.remaining -= sent;
                    controller.output_rate.remaining -= sent;
                }
                terminal.controller = Some(controller);
            }
        }
        frames
    }
}

impl Terminal {
    fn input(&mut self, payload: serde_json::Value, now: Instant) -> Result<u64, OperatorRefusal> {
        if self.status != TerminalStatus::Running {
            return Err(OperatorRefusal::TerminalLost);
        }
        let event: TerminalEvent =
            serde_json::from_value(payload).map_err(|_| OperatorRefusal::InvalidRequest)?;
        let TerminalEvent::Data { offset, bytes_b64 } = event else {
            return Err(OperatorRefusal::InvalidRequest);
        };
        if bytes_b64.len() > CHUNK_BYTES.div_ceil(3) * 4 {
            return Err(OperatorRefusal::InvalidRequest);
        }
        let bytes = STANDARD
            .decode(&bytes_b64)
            .map_err(|_| OperatorRefusal::InvalidRequest)?;
        if bytes.is_empty() || bytes.len() > CHUNK_BYTES {
            return Err(OperatorRefusal::InvalidRequest);
        }
        if offset < self.input_next {
            return if self
                .acknowledged
                .iter()
                .any(|(at, prior)| *at == offset && *prior == bytes)
            {
                Ok(self.input_next)
            } else {
                Err(OperatorRefusal::Conflict)
            };
        }
        if offset != self.input_next {
            return Err(OperatorRefusal::OffsetGap);
        }
        if bytes.len() as u64
            > self
                .input_rate
                .available(now, Duration::from_secs(1), WINDOW_BYTES)
        {
            return Err(OperatorRefusal::Backpressure);
        }
        self.process
            .as_mut()
            .ok_or(OperatorRefusal::TerminalLost)?
            .input(bytes.clone())?;
        self.input_rate.remaining -= bytes.len() as u64;
        self.input_next += bytes.len() as u64;
        self.last_input = now;
        self.acknowledged_bytes += bytes.len();
        self.acknowledged.push_back((offset, bytes));
        while self.acknowledged_bytes > 64 * 1024 {
            if let Some((_, bytes)) = self.acknowledged.pop_front() {
                self.acknowledged_bytes -= bytes.len();
            }
        }
        Ok(self.input_next)
    }

    fn output(
        &self,
        instance: &OperatorId,
        controller: &mut Controller,
        budget: &mut usize,
        frames: &mut Vec<StreamFrame>,
    ) {
        let output = lock(&self.output);
        if controller.credit == 0 {
            return;
        }
        if controller.cursor < output.oldest() {
            controller.cursor = output.oldest();
            frames.push(controller.event(TerminalEvent::OutputGap {
                oldest: TerminalCursor {
                    instance: instance.clone(),
                    offset: output.oldest(),
                },
            }));
        }
        let available = usize::try_from(
            (output.next - controller.cursor)
                .min(controller.credit)
                .min(CHUNK_BYTES as u64)
                .min(*budget as u64),
        )
        .unwrap_or(0);
        if available > 0 {
            let start = usize::try_from(controller.cursor - output.oldest()).unwrap_or(RING_BYTES);
            let bytes: Vec<_> = output
                .bytes
                .iter()
                .skip(start)
                .take(available)
                .copied()
                .collect();
            frames.push(controller.event(TerminalEvent::Data {
                offset: controller.cursor,
                bytes_b64: STANDARD.encode(bytes),
            }));
            controller.cursor += available as u64;
            controller.credit -= available as u64;
            *budget -= available;
        }
        if matches!(
            self.status,
            TerminalStatus::Exited | TerminalStatus::TerminationUnknown
        ) && controller.cursor == output.next
            && !controller.exit_sent
        {
            frames.push(controller.event(TerminalEvent::Exit {
                status: self.status,
                code: self.exit_code,
            }));
            controller.exit_sent = true;
        }
    }
}

impl Controller {
    fn frame(&mut self, event: StreamEvent) -> StreamFrame {
        let frame = StreamFrame {
            stream: self.stream,
            seq: self.outbound_seq,
            event,
        };
        self.outbound_seq += 1;
        frame
    }

    fn event(&mut self, event: TerminalEvent) -> StreamFrame {
        self.frame(StreamEvent::Data {
            payload: serde_json::to_value(event).unwrap_or_default(),
        })
    }
}
