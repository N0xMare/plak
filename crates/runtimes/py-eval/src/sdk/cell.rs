//! Drives one cell on its checkout: the bootstrap, the type-checked feed, and
//! every suspension until a terminal.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use monty_pool::{Checkout, PoolError, ResumeValue, TurnEvent, on_print_sync};
use monty_types::{ExcType, MontyException, MontyObject, MontyUuid, NameLookupResult, NamedValues};
use plakous_agent::internal::LocalCompositionSessionLoss;
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};

use super::{
    args,
    host::{self, HostReply},
    render::Terminal,
    scope::{Ack, CellScope, StopReason},
};

/// The private module the bootstrap imports.
const HOST_MODULE: &str = "_sdk_host";
/// Identity of the `_sdk_host` module object.
const HOST_MODULE_ID: u128 = 0x5d4b_0057_0000_4000_8000_0000_0000_0001;
/// Printed output kept per cell; the model sees a bounded view of it.
const MAX_PRINTED_BYTES: usize = 1_048_576;
const PRINT_CAP_NOTICE: &str = "\n[printed output past 1 MiB was not kept]\n";

/// Printed output, capped once.
#[derive(Default)]
pub(super) struct Printed {
    pub(super) text: String,
    capped: bool,
}

impl Printed {
    fn push(&mut self, piece: &str) {
        if self.capped {
            return;
        }
        let room = MAX_PRINTED_BYTES.saturating_sub(self.text.len());
        if piece.len() <= room {
            self.text.push_str(piece);
            return;
        }
        let mut end = room;
        while !piece.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&piece[..end]);
        self.text.push_str(PRINT_CAP_NOTICE);
        self.capped = true;
    }
}

/// How a drive ended: a renderable terminal, or a host loss.
pub(super) enum Drive {
    Terminal(Terminal),
    Lost(LocalCompositionSessionLoss),
}

/// Feeds the bootstrap with type checking skipped, answering its one import.
pub(super) async fn bootstrap(
    checkout: &mut Checkout,
    source: &str,
    host_functions: &[String],
) -> Result<(), String> {
    let on_print = &mut on_print_sync(|_, _: &str| {});
    let mut turn = checkout
        .feed(source, NamedValues::new(), Vec::new(), true, on_print)
        .await;
    loop {
        let event = turn.map_err(|error| format!("SDK bootstrap failed: {error}"))?;
        turn = match event {
            TurnEvent::Complete(_) => return Ok(()),
            TurnEvent::OsCall {
                function_name,
                args,
                ..
            } if function_name == "import"
                && args.arg(0).and_then(|name| name.as_str()) == Some(HOST_MODULE) =>
            {
                let module = MontyObject::module(
                    HOST_MODULE,
                    MontyUuid::from_u128(HOST_MODULE_ID),
                    host_functions.iter().map(|name| {
                        (
                            MontyObject::string(name.as_str()),
                            MontyObject::function(name.as_str(), None),
                        )
                    }),
                );
                checkout.resume(ResumeValue::Return(module), on_print).await
            }
            other => return Err(format!("SDK bootstrap suspended unexpectedly: {other:?}")),
        };
    }
}

/// One cell's drive state.
pub(super) struct CellDrive<'a> {
    pub(super) checkout: &'a mut Checkout,
    pub(super) printed: &'a mut Printed,
    scope: Arc<CellScope>,
    host_functions: &'a BTreeSet<String>,
    tasks: JoinSet<()>,
    results: mpsc::UnboundedReceiver<(u32, HostReply)>,
    results_tx: mpsc::UnboundedSender<(u32, HostReply)>,
    ready: BTreeMap<u32, HostReply>,
    issued: BTreeSet<u32>,
    stop: watch::Receiver<Option<StopReason>>,
}

impl Drop for CellDrive<'_> {
    fn drop(&mut self) {
        self.tasks.abort_all();
    }
}

impl<'a> CellDrive<'a> {
    pub(super) fn new(
        checkout: &'a mut Checkout,
        printed: &'a mut Printed,
        scope: Arc<CellScope>,
        host_functions: &'a BTreeSet<String>,
    ) -> Self {
        let (results_tx, results) = mpsc::unbounded_channel();
        let stop = scope.stop_signal();
        Self {
            checkout,
            printed,
            scope,
            host_functions,
            tasks: JoinSet::new(),
            results,
            results_tx,
            ready: BTreeMap::new(),
            issued: BTreeSet::new(),
            stop,
        }
    }

    /// Runs the cell source to a terminal.
    pub(super) async fn run(&mut self, source: &str) -> Drive {
        let fed = {
            let printed = &mut *self.printed;
            let on_print = &mut on_print_sync(|_, text: &str| printed.push(text));
            self.checkout
                .feed(source, NamedValues::new(), Vec::new(), false, on_print)
                .await
        };
        let mut turn = match fed {
            Err(PoolError::Typing(diagnostics)) => {
                return Drive::Terminal(Terminal::TypeErrors(diagnostics));
            }
            other => other,
        };
        loop {
            let event = match turn {
                Ok(event) => event,
                Err(error) => return classify(error),
            };
            turn = match event {
                TurnEvent::Complete(value) => return Drive::Terminal(Terminal::Completed(value)),
                TurnEvent::FunctionCall {
                    function_name,
                    args,
                    call_id,
                    object_id,
                    ..
                } => {
                    if let Some(reason) = self.scope.stop_reason() {
                        return Drive::Terminal(Terminal::Stopped(reason));
                    }
                    let answer = self.call(&function_name, &args, call_id, object_id.is_some());
                    self.resume(answer).await
                }
                TurnEvent::OsCall { function_name, .. } => {
                    // After the bootstrap every import is `ModuleNotFoundError`
                    // and every other OS call has no handler.
                    let answer = if function_name == "import" {
                        ResumeValue::NotFound
                    } else {
                        ResumeValue::NotHandled
                    };
                    self.resume(answer).await
                }
                TurnEvent::NameLookup { .. } => {
                    let printed = &mut *self.printed;
                    let on_print = &mut on_print_sync(|_, text: &str| printed.push(text));
                    self.checkout
                        .resume_name_lookup(NameLookupResult::Undefined, on_print)
                        .await
                }
                TurnEvent::ResolveFutures {
                    pending_call_ids, ..
                } => match self.collect(&pending_call_ids).await {
                    Ok(batch) => self.resume_batch(batch).await,
                    Err(drive) => return drive,
                },
            };
        }
    }

    /// Answers one `FunctionCall`: a bound host function with valid
    /// arguments becomes a future resolved by a host task.
    fn call(
        &mut self,
        name: &str,
        call: &monty_types::CallArgs,
        call_id: u32,
        method: bool,
    ) -> ResumeValue {
        if method || !self.host_functions.contains(name) {
            return ResumeValue::NotFound;
        }
        let host_call = match args::parse(name, call) {
            Ok(host_call) => host_call,
            Err(error) => return ResumeValue::Error(error.into_exception()),
        };
        let scope = Arc::clone(&self.scope);
        let results = self.results_tx.clone();
        self.tasks.spawn(async move {
            let reply = host::execute(host_call, scope).await;
            let _ = results.send((call_id, reply));
        });
        self.issued.insert(call_id);
        ResumeValue::Future
    }

    async fn resume(&mut self, answer: ResumeValue) -> Result<TurnEvent, PoolError> {
        let printed = &mut *self.printed;
        let on_print = &mut on_print_sync(|_, text: &str| printed.push(text));
        self.checkout.resume(answer, on_print).await
    }

    /// Waits until at least one awaited future has its reply, then takes
    /// every awaited reply that is ready.
    async fn collect(&mut self, pending: &[u32]) -> Result<Vec<(u32, HostReply)>, Drive> {
        if let Some(unknown) = pending.iter().find(|id| !self.issued.contains(id)) {
            return Err(Drive::Lost(LocalCompositionSessionLoss::ProtocolViolation(
                format!("monty awaited unknown SDK call {unknown}"),
            )));
        }
        loop {
            if let Some(reason) = *self.stop.borrow() {
                return Err(Drive::Terminal(Terminal::Stopped(reason)));
            }
            let ready = pending
                .iter()
                .filter_map(|id| self.ready.remove_entry(id))
                .collect::<Vec<_>>();
            if !ready.is_empty() {
                return Ok(ready);
            }
            tokio::select! {
                _ = self.stop.changed() => {}
                reply = self.results.recv() => match reply {
                    Some((id, reply)) => {
                        self.ready.insert(id, reply);
                    }
                    None => {
                        return Err(Drive::Lost(LocalCompositionSessionLoss::HostLost(
                            "SDK call results closed".into(),
                        )));
                    }
                },
            }
        }
    }

    /// Resolves ready futures, acknowledging each delivered result only after
    /// the worker accepted the batch. A batch too large for one frame is
    /// split, delivering the first reply alone.
    async fn resume_batch(&mut self, batch: Vec<(u32, HostReply)>) -> Result<TurnEvent, PoolError> {
        let mut values: Vec<(u32, ResumeValue)> = Vec::with_capacity(batch.len());
        let mut acks: Vec<Ack> = Vec::new();
        for (id, reply) in batch {
            let value = match reply.value {
                Ok(value) => ResumeValue::Return(value),
                Err(exception) => ResumeValue::Error(exception),
            };
            values.push((id, value));
            if values.len() > 1 && crate::validate_futures_batch(&values).is_err() {
                let Some((id, value)) = values.pop() else {
                    break;
                };
                let value = match value {
                    ResumeValue::Error(exception) => Err(exception),
                    ResumeValue::Return(value) => Ok(value),
                    _ => Err(host::runtime_error("SDK result is not terminal")),
                };
                self.ready.insert(
                    id,
                    HostReply {
                        value,
                        ack: reply.ack,
                    },
                );
                continue;
            }
            acks.extend(reply.ack);
        }
        if crate::validate_futures_batch(&values).is_err()
            && let Some((id, _)) = values.pop()
        {
            values.push((
                id,
                ResumeValue::Error(host::runtime_error("SDK result exceeds the wire frame")),
            ));
        }
        let resumed = {
            let printed = &mut *self.printed;
            let on_print = &mut on_print_sync(|_, text: &str| printed.push(text));
            self.checkout.resume_futures(values, on_print).await
        };
        let delivery = crate::batch_acceptance(&resumed, self.checkout.pid());
        for ack in acks {
            ack.send(delivery.clone());
        }
        resumed
    }
}

/// Maps a turn error to the cell's terminal.
fn classify(error: PoolError) -> Drive {
    match error {
        PoolError::Runtime(exception) => Drive::Terminal(exception_terminal(&exception)),
        PoolError::Timeout { .. } => Drive::Terminal(Terminal::Stopped(StopReason::Compute)),
        PoolError::Protocol(context)
            if context.starts_with(crate::MONTY_UNDECODABLE_EXCEPTION_CONTEXT) =>
        {
            Drive::Terminal(Terminal::Failed(format!(
                "{}{context}",
                crate::prompts::PLAKOUS_PY_PTC_ERROR_UNDECODABLE_TRACEBACK_PREFIX
            )))
        }
        other => Drive::Lost(LocalCompositionSessionLoss::HostLost(other.to_string())),
    }
}

/// A raised exception is a failure unless it is a limit's own exception.
fn exception_terminal(exception: &MontyException) -> Terminal {
    let message = exception.message().unwrap_or_default();
    let reason = match exception.exc_type() {
        ExcType::MemoryError
            if message.starts_with("memory limit exceeded")
                || message.starts_with("the worker exceeded its memory limit") =>
        {
            Some(StopReason::Memory)
        }
        ExcType::TimeoutError if message.contains("time limit exceeded") => {
            Some(StopReason::Compute)
        }
        ExcType::RuntimeError
            if message.starts_with("suspension limit ") && message.ends_with(" exceeded") =>
        {
            Some(StopReason::Suspensions)
        }
        _ => None,
    };
    reason.map_or_else(
        || Terminal::Failed(exception.to_string()),
        Terminal::Stopped,
    )
}
