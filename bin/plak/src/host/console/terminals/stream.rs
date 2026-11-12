use super::{
    Arc, Budget, CHUNK, Command, Console, HostRequestContext, HostResponse, Instant, PoisonError,
    RunnerDimensions, RunnerEvent, Stream, TerminalController, TerminalError, VecDeque, WINDOW,
    dto, mapping, mpsc, output_bytes, owner, watch,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use tokio::sync::oneshot;

impl Console {
    pub(super) fn start_terminal(
        self: &Arc<Self>,
        context: HostRequestContext,
        terminal: dto::TerminalRef,
        input_next: u64,
        controller: TerminalController,
    ) -> Result<u64, dto::ReadRefusal> {
        let (commands, incoming) = mpsc::channel(8);
        let (resize, sizes) = watch::channel(None);
        let id = {
            let mut state = self
                .terminals
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            state
                .streams
                .retain(|_, stream| !stream.ended || !stream.events.is_empty());
            let active: Vec<_> = state
                .streams
                .values()
                .map(|stream| stream.owner.0.clone())
                .collect();
            state.output.retain(|principal, budget| {
                active.contains(principal)
                    || budget.available(Instant::now(), 512 * 1024) < f64::from(512 * 1024)
            });
            if state.output.len() >= 1024 && !state.output.contains_key(&context.principal) {
                return Err(dto::ReadRefusal::TerminalBackpressure {
                    retry_after_ms: None,
                });
            }
            state
                .output
                .entry(context.principal.clone())
                .or_insert_with(|| Budget::new(512 * 1024));
            if state
                .streams
                .values()
                .filter(|s| s.owner.0 == context.principal && !s.ended)
                .count()
                >= 4
                || state.streams.values().filter(|s| !s.ended).count() >= 16
                || state.streams.len() >= 32
            {
                return Err(dto::ReadRefusal::TerminalBackpressure {
                    retry_after_ms: None,
                });
            }
            state.next = state.next.checked_add(1).ok_or_else(mapping::invalid)?;
            let id = state.next;
            state.notifications.entry(owner(&context)).or_default();
            state.streams.insert(
                id,
                Stream {
                    owner: owner(&context),
                    terminal,
                    commands,
                    resize,
                    events: VecDeque::new(),
                    seq: 0,
                    sent_output: 0,
                    next_input: input_next,
                    acknowledged_input: input_next,
                    input_tokens: f64::from(65_536),
                    input_updated: Instant::now(),
                    ended: false,
                    ready: false,
                    output: Budget::new(256 * 1024),
                },
            );
            id
        };
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.pump_terminal(id, context, controller, incoming, sizes)
                .await;
        });
        Ok(id)
    }

    pub(super) async fn terminal_input(
        &self,
        context: &HostRequestContext,
        id: u64,
        offset: u64,
        encoded: &str,
    ) -> Result<HostResponse, dto::ReadRefusal> {
        if encoded.len() > CHUNK.div_ceil(3) * 4 {
            return Err(mapping::invalid());
        }
        let bytes = STANDARD.decode(encoded).map_err(|_| mapping::invalid())?;
        if bytes.is_empty() || bytes.len() > CHUNK || STANDARD.encode(&bytes) != encoded {
            return Err(mapping::invalid());
        }
        let (reply, received) = oneshot::channel();
        {
            let mut state = self
                .terminals
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let stream = state
                .streams
                .get_mut(&id)
                .filter(|s| s.owner == owner(context) && !s.ended)
                .ok_or(dto::ReadRefusal::TerminalLost {})?;
            if offset != stream.next_input || stream.next_input != stream.acknowledged_input {
                return Err(dto::ReadRefusal::TerminalBackpressure {
                    retry_after_ms: None,
                });
            }
            let now = Instant::now();
            stream.input_tokens = (stream.input_tokens
                + now.duration_since(stream.input_updated).as_secs_f64() * f64::from(65_536))
            .min(f64::from(65_536));
            stream.input_updated = now;
            if stream.input_tokens
                < f64::from(u32::try_from(bytes.len()).map_err(|_| mapping::invalid())?)
            {
                return Err(dto::ReadRefusal::TerminalBackpressure {
                    retry_after_ms: None,
                });
            }
            let permit = stream.commands.clone().try_reserve_owned().map_err(|_| {
                dto::ReadRefusal::TerminalBackpressure {
                    retry_after_ms: None,
                }
            })?;
            let next = offset
                .checked_add(bytes.len() as u64)
                .ok_or_else(mapping::invalid)?;
            stream.input_tokens -=
                f64::from(u32::try_from(bytes.len()).map_err(|_| mapping::invalid())?);
            stream.next_input = next;
            permit.send(Command::Input {
                offset,
                bytes,
                reply,
            });
        }
        match received.await {
            Ok(Ok(())) => Ok(HostResponse::TerminalAccepted {}),
            Ok(Err(reason)) => Err(reason),
            Err(_) => Ok(HostResponse::OperationOutcomeUnknown {}),
        }
    }

    pub(super) fn terminal_credit(
        &self,
        context: &HostRequestContext,
        grants: Vec<dto::TerminalCredit>,
    ) -> Result<HostResponse, dto::ReadRefusal> {
        if grants.is_empty() || grants.len() > 4 {
            return Err(mapping::invalid());
        }
        let mut state = self
            .terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut permits = Vec::new();
        for (index, grant) in grants.iter().enumerate() {
            if grant.bytes == 0
                || grant.bytes > WINDOW
                || grants[..index].iter().any(|g| g.stream == grant.stream)
            {
                return Err(mapping::invalid());
            }
            let stream = state
                .streams
                .get(&grant.stream)
                .filter(|s| s.owner == owner(context) && !s.ended)
                .ok_or(dto::ReadRefusal::TerminalLost {})?;
            if grant.bytes > stream.sent_output {
                return Err(dto::ReadRefusal::TerminalBackpressure {
                    retry_after_ms: None,
                });
            }
            permits.push(stream.commands.clone().try_reserve_owned().map_err(|_| {
                dto::ReadRefusal::TerminalBackpressure {
                    retry_after_ms: None,
                }
            })?);
        }
        for (grant, permit) in grants.into_iter().zip(permits) {
            if let Some(stream) = state.streams.get_mut(&grant.stream) {
                stream.sent_output -= grant.bytes;
            }
            permit.send(Command::Credit(grant.bytes));
        }
        Ok(HostResponse::TerminalAccepted {})
    }

    pub(super) fn publish_terminal(&self, id: u64, event: dto::TerminalEvent) -> bool {
        let mut state = self
            .terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(stream) = state.streams.get_mut(&id) else {
            return false;
        };
        if output_bytes(&event).is_none() {
            return false;
        }
        let ended = matches!(
            event,
            dto::TerminalEvent::Exit { .. } | dto::TerminalEvent::Suspended {}
        );
        if stream.ended {
            return ended;
        }
        if !ended && !queue_terminal_event(stream, &event) {
            return false;
        }
        if let dto::TerminalEvent::InputAccepted { next } = event {
            stream.acknowledged_input = next;
        }
        stream.ended = ended;
        if ended || !coalesce_data(&mut stream.events, &event) {
            stream.events.push_back(event);
        }
        let owner = stream.owner.clone();
        if let Some(notify) = state.notifications.get(&owner) {
            notify.notify_one();
        }
        true
    }

    async fn pump_terminal(
        self: Arc<Self>,
        id: u64,
        context: HostRequestContext,
        mut controller: TerminalController,
        mut commands: mpsc::Receiver<Command>,
        mut sizes: watch::Receiver<Option<RunnerDimensions>>,
    ) {
        type Pending = Option<(u64, u64, oneshot::Sender<Result<(), dto::ReadRefusal>>)>;
        let mut pending: Pending = None;
        if controller.credit(&context, WINDOW).await.is_err() {
            close_commands(&mut commands);
            self.publish_terminal(id, dto::TerminalEvent::Suspended {});
            return;
        }
        let mut pulse = tokio::time::interval(std::time::Duration::from_millis(50));
        loop {
            enum Work {
                Command(Option<Command>),
                Event(Result<RunnerEvent, TerminalError>),
                Resize,
                Pulse,
            }
            let work = tokio::select! {
                biased;
                command = commands.recv() => Work::Command(command),
                changed = sizes.changed() => { if changed.is_err() { break } Work::Resize },
                event = controller.next(&context) => Work::Event(event),
                _ = pulse.tick() => Work::Pulse,
            };
            match work {
                Work::Command(None) => break,
                Work::Command(Some(Command::Input {
                    offset,
                    bytes,
                    reply,
                })) => {
                    if controller.input(&context, offset, &bytes).await.is_err() {
                        break;
                    }
                    pending = Some((offset, offset + bytes.len() as u64, reply));
                }
                Work::Command(Some(Command::Credit(bytes))) => {
                    if controller.credit(&context, bytes).await.is_err() {
                        break;
                    }
                }
                Work::Resize => {
                    let size = *sizes.borrow_and_update();
                    if let Some(size) = size
                        && controller.resize(&context, size).await.is_err()
                    {
                        break;
                    }
                }
                Work::Event(Ok(event)) => {
                    match &event {
                        RunnerEvent::InputAccepted { next } => {
                            if let Some((_, expected, reply)) = pending.take() {
                                if *next != expected {
                                    break;
                                }
                                let _ = reply.send(Ok(()));
                            }
                        }
                        RunnerEvent::Refused { refusal } => {
                            if let Some((offset, _, reply)) = pending.take() {
                                if let Some(stream) = self
                                    .terminals
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .streams
                                    .get_mut(&id)
                                {
                                    stream.next_input = offset;
                                }
                                let _ = reply
                                    .send(Err(mapping::refusal(TerminalError::Refused(*refusal))));
                            }
                        }
                        _ => {}
                    }
                    let ended = matches!(event, RunnerEvent::Exit { .. });
                    if !self.publish_terminal(id, mapping::event(event)) || ended {
                        break;
                    }
                }
                Work::Event(Err(_)) => break,
                Work::Pulse => {
                    if !self.pulse_terminal(id) {
                        break;
                    }
                }
            }
        }
        close_commands(&mut commands);
        self.finish_terminal(id, controller).await;
    }

    async fn finish_terminal(&self, id: u64, controller: TerminalController) {
        let ended = self
            .terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .streams
            .get(&id)
            .is_none_or(|s| s.ended);
        if !ended {
            self.publish_terminal(id, dto::TerminalEvent::Suspended {});
        }
        drop(controller);
        while self.terminal_pending(id) {
            self.pulse_terminal(id);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    fn pulse_terminal(&self, id: u64) -> bool {
        let state = self
            .terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(stream) = state.streams.get(&id) else {
            return false;
        };
        if !stream.events.is_empty()
            && let Some(notify) = state.notifications.get(&stream.owner)
        {
            notify.notify_one();
        }
        true
    }

    fn terminal_pending(&self, id: u64) -> bool {
        self.terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .streams
            .get(&id)
            .is_some_and(|s| !s.events.is_empty())
    }
}

fn close_commands(commands: &mut mpsc::Receiver<Command>) {
    commands.close();
    while let Ok(command) = commands.try_recv() {
        if let Command::Input { reply, .. } = command {
            let _ = reply.send(Err(dto::ReadRefusal::TerminalLost {}));
        }
    }
}

fn queue_terminal_event(stream: &mut Stream, event: &dto::TerminalEvent) -> bool {
    let bytes = stream
        .events
        .iter()
        .filter_map(output_bytes)
        .map(u64::from)
        .sum::<u64>();
    if bytes + u64::from(output_bytes(event).unwrap_or(0)) > WINDOW {
        return false;
    }
    // Control state is latest-only until assigned a sequence by the sole writer.
    if matches!(event, dto::TerminalEvent::OutputGap { .. }) {
        // A reset preceding queued bytes cannot be moved behind those bytes.
        let after_data = stream
            .events
            .iter()
            .rposition(|event| matches!(event, dto::TerminalEvent::Data { .. }))
            .map_or(0, |index| index + 1);
        let mut index = 0;
        stream.events.retain(|queued| {
            let keep =
                index < after_data || !matches!(queued, dto::TerminalEvent::OutputGap { .. });
            index += 1;
            keep
        });
    } else if !matches!(event, dto::TerminalEvent::Data { .. }) {
        stream
            .events
            .retain(|queued| std::mem::discriminant(queued) != std::mem::discriminant(event));
    }
    true
}

fn coalesce_data(events: &mut VecDeque<dto::TerminalEvent>, event: &dto::TerminalEvent) -> bool {
    let Some(dto::TerminalEvent::Data {
        offset: prior,
        bytes_b64: encoded,
    }) = events.back_mut()
    else {
        return false;
    };
    let dto::TerminalEvent::Data { offset, bytes_b64 } = event else {
        return false;
    };
    let (Ok(mut bytes), Ok(next)) = (STANDARD.decode(&*encoded), STANDARD.decode(bytes_b64)) else {
        return false;
    };
    if *prior + bytes.len() as u64 != *offset || bytes.len() + next.len() > CHUNK {
        return false;
    }
    bytes.extend(next);
    *encoded = STANDARD.encode(bytes);
    true
}
