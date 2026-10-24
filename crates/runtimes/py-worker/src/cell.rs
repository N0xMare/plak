//! The worker loop: cell protocol frames in, one upstream Monty `Child` driven
//! in process, cell protocol frames out.
//!
//! Each `Child::handle` call runs the interpreter to its next suspension.
//! Prints stream out as `Output` while it runs; the turn-ending event is then
//! answered here: a bound tool call becomes a `NestedCall` and resumes as a
//! future, OS calls and unknown names are refused, and a wait on futures
//! becomes `Waiting` until the host has delivered every result of the batch,
//! which resumes the interpreter with all of them and accepts each one.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io,
    process::ExitCode,
};

use monty_proto::{
    FrameError, WireFunctionCall, ext_result_to_proto, future_results_from_proto,
    future_results_to_proto, os_call_from_proto, pb,
    worker::{Child, EventSink, HandleOutcome},
};
use monty_types::{
    AssertMessageAnnotations, DateTimeSource, ExcType, ExtFunctionResult, MontyException,
    MontyObject, NameLookupResult, OsPolicy, RandomStart, ResourceLimits, SleepMode,
    TypeCheckingConfig, memory_limit_with_headroom,
};
use plakous_cell::{
    BindingInput, CELL_PROTOCOL_REVISION, CellOutcome, CellPrelude, CellValue, DeliveryVerdict,
    EngineConfig, HostFrame, LossClass, MAX_CELL_VALUE_DEPTH, MAX_FRAME_BYTES, MONTY_WORKER,
    MontyBootstrap, OutputItem, WorkerFrame, json_depth, read_frame_blocking, sample_self_rusage,
    write_frame_blocking,
};

use crate::codec::{call_to_json, format_monty, nested_outcome_to_result};
use crate::prompts::{
    PLAKOUS_PY_PTC_ERROR_OUTSTANDING_NESTED_PREFIX, PLAKOUS_PY_PTC_ERROR_OUTSTANDING_NESTED_SUFFIX,
    PLAKOUS_PY_PTC_ERROR_UNDECODABLE_TRACEBACK_PREFIX,
};
use crate::test_hooks::boundary;

// Leave room for the longest status/header (including a u64 cell id), so a
// cumulative-cap notice survives the facade's status-inclusive 1 MiB bound.
const MAX_PRINT_BYTES: usize = 1_048_576 - 64;
const PRINT_NOTICE: &str = "\n[Python output truncated at 1 MiB]\n";
const MAX_OUTSTANDING_NESTED: usize = 16;
const SCRIPT_NAME: &str = "plak-py-ptc.py";

/// A loss ends the worker: the frame it sends and the reason.
struct Lost {
    class: LossClass,
    message: String,
}

impl Lost {
    fn protocol(message: impl Into<String>) -> Self {
        Self {
            class: LossClass::ProtocolViolation,
            message: message.into(),
        }
    }

    fn engine(message: impl Into<String>) -> Self {
        Self {
            class: LossClass::EngineFailure,
            message: message.into(),
        }
    }
}

fn write(frame: &WorkerFrame) -> Result<(), Lost> {
    write_frame_blocking(&mut io::stdout(), frame).map_err(Lost::engine)
}

/// The worker's entry point on its large-stack thread.
pub(crate) fn serve() -> ExitCode {
    let hello = WorkerFrame::Hello {
        revision: CELL_PROTOCOL_REVISION.to_owned(),
        engine: MONTY_WORKER.engine.to_owned(),
        adapter: MONTY_WORKER.adapter.to_owned(),
    };
    if write(&hello).is_err() {
        return ExitCode::from(3);
    }
    let mut stdin = io::stdin().lock();
    let result = configure(&mut stdin).and_then(|child| {
        let mut worker = Worker {
            child,
            bindings: None,
            running: None,
        };
        worker.serve(&mut stdin)
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(lost) => {
            eprintln!("plakous-py-host: {}", lost.message);
            let _ = write(&WorkerFrame::HostLost {
                class: lost.class,
                message: lost.message,
            });
            ExitCode::from(4)
        }
    }
}

/// Reads `Configure`, checks this protocol and these limits, and configures
/// the interpreter session.
fn configure(stdin: &mut impl io::Read) -> Result<Child, Lost> {
    let frame = read_frame_blocking(stdin, MAX_FRAME_BYTES)
        .map_err(Lost::protocol)?
        .ok_or_else(|| Lost::protocol("parent closed before configure"))?;
    match serde_json::from_slice::<HostFrame>(&frame) {
        Ok(HostFrame::Configure { revision, engine })
            if revision == CELL_PROTOCOL_REVISION && engine == EngineConfig::MONTY => {}
        Ok(HostFrame::Configure { .. }) => {
            return Err(Lost::protocol("configure names another revision or limits"));
        }
        _ => return Err(Lost::protocol("first parent frame is not configure")),
    }
    let mut child = Child::default();
    let mut sink = TurnSink::default();
    child
        .handle(
            request(pb::parent_request::Kind::Configure(session_config())),
            &mut sink,
        )
        .map_err(|error| Lost::engine(error.to_string()))?;
    apply_memory_limit(&child);
    match sink.turn_end.and_then(|event| event.kind) {
        Some(pb::child_event::Kind::Ok(_)) => Ok(child),
        _ => Err(Lost::engine(
            "the interpreter refused its session configuration",
        )),
    }
}

/// The interpreter session: a 256 MiB soft memory limit, no suspension cap
/// (it would count across every cell of the session's life; outstanding
/// nested calls are bounded instead), clock and `random` answered inside the
/// interpreter, and sleeps surfaced as OS calls, which are refused.
fn session_config() -> pb::Configure {
    let EngineConfig::Monty { memory_bytes } = EngineConfig::MONTY else {
        unreachable!("the Monty engine configuration names Monty limits")
    };
    let limits = ResourceLimits {
        max_memory: usize::try_from(memory_bytes).ok(),
        max_suspensions: usize::MAX,
        ..ResourceLimits::default()
    };
    let os_policy = OsPolicy {
        datetime: DateTimeSource::System,
        random_start: RandomStart::System,
        sleep: SleepMode::CallHost,
        ..OsPolicy::default()
    };
    let type_check = TypeCheckingConfig::default();
    pb::Configure {
        script_name: SCRIPT_NAME.to_owned(),
        limits: Some((&limits).into()),
        type_check: false,
        type_check_stubs: None,
        type_check_format: pb::TypeCheckFormat::from(type_check.format).into(),
        type_check_color: type_check.color,
        assert_message_annotations: Some(AssertMessageAnnotations::default().max_bytes()),
        protocol_version: monty_proto::PROTOCOL_VERSION,
        monty_version: monty_types::MONTY_VERSION.to_owned(),
        print_flush_interval_ms: None,
        os_policy: Some((&os_policy).into()),
        persistence: pb::Persistence::Unspecified.into(),
        profile: None,
    }
}

fn request(kind: pb::parent_request::Kind) -> pb::ParentRequest {
    pb::ParentRequest {
        kind: Some(kind),
        ..pb::ParentRequest::default()
    }
}

/// Applies the hard memory limit of the session the child holds: the
/// interpreter's soft limit plus upstream's headroom.
fn apply_memory_limit(child: &Child) {
    let budget = child.session_budget();
    let hard_memory_limit = memory_limit_with_headroom(budget.max_memory, budget.type_check);
    if let Err(error) = monty_alloc::set_hard_limit(hard_memory_limit) {
        panic!("Monty worker must install LimitedAllocator globally: {error}");
    }
}

/// One private host function the bootstrap defined.
struct Binding {
    tool: String,
    input: BindingInput,
}

struct Worker {
    child: Child,
    bindings: Option<HashMap<String, Binding>>,
    running: Option<Running>,
}

/// The running cell: its print budget, the futures it holds, the results the
/// host delivered for them, and the batch it waits on.
#[derive(Default)]
struct Running {
    cell: u64,
    printed: Printed,
    futures: BTreeSet<u32>,
    results: BTreeMap<u32, Result<CellValue, String>>,
    waiting: Option<Vec<u32>>,
    accepted: Vec<u32>,
}

#[derive(Default)]
struct Printed {
    bytes: usize,
    truncated: bool,
}

impl Printed {
    /// The part of `text` the cell's 1 MiB print budget admits, with the
    /// truncation notice the first time the budget runs out.
    fn admit(&mut self, text: &str) -> Option<String> {
        if self.truncated {
            return None;
        }
        let room = (MAX_PRINT_BYTES - PRINT_NOTICE.len()).saturating_sub(self.bytes);
        let mut end = text.len().min(room);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut admitted = text[..end].to_owned();
        if end < text.len() {
            admitted.push_str(PRINT_NOTICE);
            self.truncated = true;
        }
        self.bytes += end;
        (!admitted.is_empty()).then_some(admitted)
    }
}

/// Streams prints of the running cell as `Output` and keeps the turn-ending
/// event for the loop.
#[derive(Default)]
struct TurnSink<'a> {
    cell: Option<(u64, Option<&'a mut Printed>)>,
    turn_end: Option<pb::ChildEvent>,
    accepted: Vec<u32>,
}

impl EventSink for TurnSink<'_> {
    fn send(&mut self, event: &pb::ChildEvent) -> Result<(), FrameError> {
        // A print or a normal suspension/terminal proves ResumeFutures ran.
        // Configuration/protocol failures prove no such handoff.
        if !self.accepted.is_empty()
            && !matches!(event.kind, Some(pb::child_event::Kind::FatalError(_)))
            && let Some((cell, _)) = &self.cell
        {
            for call in self.accepted.drain(..) {
                write_frame_blocking(
                    &mut io::stdout(),
                    &WorkerFrame::Delivery {
                        cell: *cell,
                        call: u64::from(call),
                        verdict: DeliveryVerdict::Accepted,
                    },
                )
                .map_err(|error| FrameError::Io(io::Error::other(error)))?;
            }
            boundary("after_accepted");
        }
        let Some(pb::child_event::Kind::Print(print)) = &event.kind else {
            self.turn_end = Some(event.clone());
            return Ok(());
        };
        let Some((cell, Some(printed))) = &mut self.cell else {
            return Ok(());
        };
        for segment in &print.segments {
            if let Some(text) = printed.admit(&segment.text) {
                let frame = WorkerFrame::Output {
                    cell: *cell,
                    item: OutputItem::Text { text },
                };
                write_frame_blocking(&mut io::stdout(), &frame)
                    .map_err(|error| FrameError::Io(io::Error::other(error)))?;
            }
        }
        Ok(())
    }
}

impl Worker {
    fn serve(&mut self, stdin: &mut impl io::Read) -> Result<(), Lost> {
        loop {
            let Some(frame) =
                read_frame_blocking(stdin, MAX_FRAME_BYTES).map_err(Lost::protocol)?
            else {
                return Ok(());
            };
            let frame = serde_json::from_slice::<HostFrame>(&frame)
                .map_err(|_| Lost::protocol("malformed host frame"))?;
            match frame {
                HostFrame::StartCell {
                    cell,
                    source,
                    prelude,
                    window: None,
                } => self.start(cell, &source, prelude)?,
                HostFrame::NestedResult {
                    cell,
                    call,
                    outcome,
                } => self.nested_result(cell, call, outcome)?,
                // A cell has no cooperative interrupt; the host stops a
                // running cell by ending the worker.
                HostFrame::Cancel { .. } => {}
                HostFrame::Shutdown => {
                    write(&WorkerFrame::ResourceReport {
                        usage: sample_self_rusage(),
                    })?;
                    return Ok(());
                }
                HostFrame::StartCell { .. } => {
                    return Err(Lost::protocol("a Monty cell has no engine-parked window"));
                }
                HostFrame::Resume { .. } => {
                    return Err(Lost::protocol("a Monty cell cannot be resumed"));
                }
                HostFrame::Configure { .. } => return Err(Lost::protocol("repeated configure")),
            }
        }
    }

    fn start(&mut self, cell: u64, source: &str, prelude: CellPrelude) -> Result<(), Lost> {
        if self.running.is_some() {
            return Err(Lost::protocol("a cell started while another runs"));
        }
        let CellPrelude::Monty { bootstrap } = prelude else {
            return Err(Lost::protocol("a Monty cell needs a Monty prelude"));
        };
        self.running = Some(Running {
            cell,
            ..Running::default()
        });
        if let Some(bootstrap) = bootstrap
            && let Err(failure) = self.bootstrap(bootstrap)
        {
            return self.done(CellOutcome::Failed {
                message: failure,
                stored: HashMap::new(),
                generation_lost: true,
            });
        }
        let feed = pb::Feed {
            code: source.to_owned(),
            inputs: Vec::new().into(),
            values: None,
            skip_type_check: false,
            cwd: "/".to_owned(),
        };
        self.run(request(pb::parent_request::Kind::Feed(feed)))
    }

    /// Defines the session's tool wrappers before its first cell. A failure
    /// costs the interpreter: its names may be half defined.
    fn bootstrap(&mut self, bootstrap: MontyBootstrap) -> Result<(), String> {
        if self.bindings.is_some() {
            return Err("Python tool bootstrap repeated".into());
        }
        let bindings = bootstrap
            .bindings
            .into_iter()
            .map(|binding| {
                (
                    binding.function,
                    Binding {
                        tool: binding.tool,
                        input: binding.input,
                    },
                )
            })
            .collect();
        self.bindings = Some(bindings);
        if bootstrap.source.is_empty() {
            return Ok(());
        }
        let feed = pb::Feed {
            code: bootstrap.source,
            inputs: Vec::new().into(),
            values: None,
            skip_type_check: false,
            cwd: "/".to_owned(),
        };
        let event = self
            .handle(request(pb::parent_request::Kind::Feed(feed)), false)
            .map_err(|lost| format!("Python tool bootstrap failed: {}", lost.message))?;
        match event.kind {
            Some(pb::child_event::Kind::Complete(_)) => Ok(()),
            Some(pb::child_event::Kind::Error(error)) => Err(format!(
                "Python tool bootstrap failed: {}",
                error
                    .exception
                    .and_then(|exception| MontyException::try_from(exception).ok())
                    .map_or_else(|| "undecodable exception".to_owned(), |exc| exc.to_string())
            )),
            _ => Err("Python tool bootstrap unexpectedly suspended".into()),
        }
    }

    /// Hands one request to the interpreter and returns its turn-ending event.
    fn handle(
        &mut self,
        request: pb::ParentRequest,
        stream_prints: bool,
    ) -> Result<pb::ChildEvent, Lost> {
        let accepted = if matches!(
            request.kind,
            Some(pb::parent_request::Kind::ResumeFutures(_))
        ) {
            self.running
                .as_mut()
                .map_or_else(Vec::new, |running| std::mem::take(&mut running.accepted))
        } else {
            Vec::new()
        };
        let mut sink = TurnSink {
            cell: self
                .running
                .as_mut()
                .map(|running| (running.cell, stream_prints.then_some(&mut running.printed))),
            turn_end: None,
            accepted,
        };
        let outcome = self.child.handle(request, &mut sink);
        let turn_end = sink.turn_end.take();
        apply_memory_limit(&self.child);
        match outcome {
            Ok(HandleOutcome::Continue | HandleOutcome::Fatal) => {}
            Ok(HandleOutcome::Shutdown) => return Err(Lost::protocol("the interpreter shut down")),
            Err(error) => return Err(Lost::engine(format!("interpreter output failed: {error}"))),
        }
        match turn_end {
            Some(pb::ChildEvent {
                kind: Some(pb::child_event::Kind::FatalError(fatal)),
                ..
            }) => Err(Lost::engine(fatal.message)),
            Some(event) => Ok(event),
            None => Err(Lost::engine(
                "the interpreter ended its turn without an event",
            )),
        }
    }

    /// Runs the cell from `request` until it waits on futures or ends.
    fn run(&mut self, mut request: pb::ParentRequest) -> Result<(), Lost> {
        loop {
            let event = self.handle(request, true)?;
            request = match event.kind {
                Some(pb::child_event::Kind::FunctionCall(call)) => self.function_call(call)?,
                Some(pb::child_event::Kind::OsCall(call)) => {
                    let (call_id, function) = os_call_from_proto(call).map_err(|error| {
                        Lost::protocol(format!("invalid OS call payload: {error}"))
                    })?;
                    os_call_refusal(call_id, function.name())
                }
                Some(pb::child_event::Kind::NameLookup(_)) => resume_name_undefined(),
                Some(pb::child_event::Kind::ResolveFutures(futures)) => {
                    let pending = futures.pending_call_ids.into_inner();
                    return self.wait_on(pending);
                }
                Some(pb::child_event::Kind::Complete(complete)) => {
                    let value = MontyObject::try_from(complete).map_err(|error| {
                        Lost::protocol(format!("invalid result payload: {error}"))
                    })?;
                    self.done(CellOutcome::Completed {
                        value: Some(format_monty(&value, MAX_PRINT_BYTES)),
                        stored: HashMap::new(),
                    })?;
                    return Ok(());
                }
                Some(pb::child_event::Kind::Error(error)) => {
                    let exception = error
                        .exception
                        .ok_or_else(|| Lost::protocol("error event with no exception"))?;
                    return self.done(exception_outcome(exception));
                }
                _ => return Err(Lost::protocol("unexpected interpreter event")),
            };
        }
    }

    fn done(&mut self, outcome: CellOutcome) -> Result<(), Lost> {
        self.finish(outcome, write)
    }

    fn finish(
        &mut self,
        outcome: CellOutcome,
        mut send: impl FnMut(&WorkerFrame) -> Result<(), Lost>,
    ) -> Result<(), Lost> {
        let running = self
            .running
            .take()
            .ok_or_else(|| Lost::protocol("a terminal without a running cell"))?;
        for call in running.results.keys() {
            send(&WorkerFrame::Delivery {
                cell: running.cell,
                call: u64::from(*call),
                verdict: DeliveryVerdict::NotObserved(
                    "the cell ended before the interpreter observed the result".into(),
                ),
            })?;
        }
        send(&WorkerFrame::CellDone {
            cell: running.cell,
            outcome,
        })
    }

    /// Answers one tool call of the cell: a bound tool with a valid argument
    /// becomes a nested call and a future; anything else raises in Python.
    fn function_call(&mut self, call: WireFunctionCall) -> Result<pb::ParentRequest, Lost> {
        let call_id = call.call_id;
        let function_name = call.function_name.clone();
        let args = call
            .into_call_args()
            .map_err(|error| Lost::protocol(format!("invalid call payload: {error}")))?;
        let Some(binding) = self
            .bindings
            .as_ref()
            .and_then(|bindings| bindings.get(&function_name))
        else {
            return Ok(resume_call(
                call_id,
                ExtFunctionResult::NotFound(function_name),
            ));
        };
        let raise = |exc_type, message: String| {
            ExtFunctionResult::Error(MontyException::new(exc_type, Some(message)))
        };
        let argument = args.arg(0).filter(|argument| {
            args.args().len() == 1
                && args.kwargs().len() == 0
                && match binding.input {
                    BindingInput::Dictionary => matches!(
                        monty_types::unstable::node(*argument),
                        monty_types::unstable::MontyNode::Dict(_)
                    ),
                    BindingInput::String => argument.as_str().is_some(),
                }
        });
        let Some(argument) = argument else {
            let expected = match binding.input {
                BindingInput::Dictionary => "dictionary",
                BindingInput::String => "string",
            };
            let message = format!("Python tool requires exactly one {expected} argument");
            return Ok(resume_call(call_id, raise(ExcType::TypeError, message)));
        };
        let running = self
            .running
            .as_mut()
            .ok_or_else(|| Lost::protocol("a tool call without a running cell"))?;
        if running.futures.len() >= MAX_OUTSTANDING_NESTED {
            let message = format!(
                "{PLAKOUS_PY_PTC_ERROR_OUTSTANDING_NESTED_PREFIX}{MAX_OUTSTANDING_NESTED}{PLAKOUS_PY_PTC_ERROR_OUTSTANDING_NESTED_SUFFIX}"
            );
            return Ok(resume_call(call_id, raise(ExcType::RuntimeError, message)));
        }
        let input = match call_to_json(argument) {
            Ok(input) if json_depth(&input) > MAX_CELL_VALUE_DEPTH => {
                let reason =
                    format!("Python tool argument nests deeper than {MAX_CELL_VALUE_DEPTH} levels");
                return Ok(resume_call(call_id, raise(ExcType::ValueError, reason)));
            }
            Ok(input) => input,
            Err(reason) => return Ok(resume_call(call_id, raise(ExcType::ValueError, reason))),
        };
        write(&WorkerFrame::NestedCall {
            cell: running.cell,
            call: u64::from(call_id),
            name: binding.tool.clone(),
            input: CellValue::from(input),
        })?;
        running.futures.insert(call_id);
        Ok(resume_call(call_id, ExtFunctionResult::Future(call_id)))
    }

    /// The interpreter waits on `pending`: tell the host, then resume at once
    /// if every result is already here.
    fn wait_on(&mut self, pending: Vec<u32>) -> Result<(), Lost> {
        let running = self
            .running
            .as_mut()
            .ok_or_else(|| Lost::protocol("a wait without a running cell"))?;
        if let Some(unknown) = pending.iter().find(|id| !running.futures.contains(id)) {
            return Err(Lost::protocol(format!(
                "monty requested unknown nested call id {unknown}"
            )));
        }
        write(&WorkerFrame::Waiting {
            cell: running.cell,
            calls: pending.iter().copied().map(u64::from).collect(),
        })?;
        running.waiting = Some(pending);
        self.resume_if_complete()
    }

    #[allow(
        clippy::similar_names,
        reason = "cell and call are the protocol's own field names"
    )]
    fn nested_result(
        &mut self,
        cell: u64,
        call: u64,
        outcome: Result<CellValue, String>,
    ) -> Result<(), Lost> {
        let verdict = |verdict| WorkerFrame::Delivery {
            cell,
            call,
            verdict,
        };
        let Some(running) = self.running.as_mut() else {
            return write(&verdict(DeliveryVerdict::NotObserved(
                "the cell had already ended when its result was delivered".into(),
            )));
        };
        if running.cell != cell {
            return write(&verdict(DeliveryVerdict::Refused(
                "the result names another cell".into(),
            )));
        }
        let Some(id) = u32::try_from(call)
            .ok()
            .filter(|id| running.futures.contains(id) && !running.results.contains_key(id))
        else {
            return write(&verdict(DeliveryVerdict::Refused(
                "the result names no pending call of the cell".into(),
            )));
        };
        running.results.insert(id, outcome);
        self.resume_if_complete()
    }

    /// Resumes the interpreter once the host delivered every result of the
    /// batch it waits on, accepting each result as it is handed over.
    fn resume_if_complete(&mut self) -> Result<(), Lost> {
        let Some(running) = self.running.as_mut() else {
            return Err(Lost::protocol("a wait without a running cell"));
        };
        let Some(pending) = running.waiting.as_ref() else {
            return Ok(());
        };
        if !pending.iter().all(|id| running.results.contains_key(id)) {
            return Ok(());
        }
        let pending = running.waiting.take().unwrap_or_default();
        let mut batch = Vec::with_capacity(pending.len());
        for id in &pending {
            let Some(result) = running.results.remove(id) else {
                return Err(Lost::protocol(format!(
                    "monty nested reply missing for call {id}"
                )));
            };
            running.futures.remove(id);
            match nested_outcome_to_result(result) {
                Ok(result) => batch.push((*id, result)),
                Err(reason) => {
                    // Nothing of the batch reaches the interpreter: every
                    // result the host delivered is definitely unobserved.
                    let cell = running.cell;
                    let unobserved = pending
                        .iter()
                        .chain(running.results.keys())
                        .copied()
                        .collect::<BTreeSet<_>>();
                    for call in unobserved {
                        write(&WorkerFrame::Delivery {
                            cell,
                            call: u64::from(call),
                            verdict: DeliveryVerdict::NotObserved(
                                "the batch carrying the result never reached the interpreter"
                                    .into(),
                            ),
                        })?;
                    }
                    return Err(Lost::protocol(format!(
                        "monty nested reply for call {id}: {reason}"
                    )));
                }
            }
        }
        let resume = future_results_to_proto(batch);
        if future_results_from_proto(resume.results.clone(), resume.values.clone()).is_err() {
            return Err(Lost::protocol("monty nested result failed wire validation"));
        }
        running.accepted = pending;
        boundary("before_delivery");
        self.run(request(pb::parent_request::Kind::ResumeFutures(resume)))
    }
}

fn exception_outcome(exception: pb::RaisedException) -> CellOutcome {
    match MontyException::try_from(exception) {
        Ok(exc) => CellOutcome::Failed {
            message: exc.to_string(),
            stored: HashMap::new(),
            generation_lost: false,
        },
        // The typed Error event proves the cell raised even if its diagnostics
        // cannot be decoded. Do not infer this from transport error text.
        Err(error) => CellOutcome::Failed {
            message: format!(
                "{PLAKOUS_PY_PTC_ERROR_UNDECODABLE_TRACEBACK_PREFIX}invalid exception payload: {error}"
            ),
            stored: HashMap::new(),
            generation_lost: true,
        },
    }
}

fn resume_call(call_id: u32, result: ExtFunctionResult) -> pb::ParentRequest {
    let (result, values) = ext_result_to_proto(result);
    resume_call_with(call_id, result, values)
}

fn resume_call_with(
    call_id: u32,
    result: pb::ExtFunctionResult,
    values: Option<monty_proto::WireArena>,
) -> pb::ParentRequest {
    request(pb::parent_request::Kind::ResumeCall(pb::ResumeCall {
        call_id,
        result: Some(result),
        values,
    }))
}

fn resume_name_undefined() -> pb::ParentRequest {
    request(pb::parent_request::Kind::ResumeNameLookup(
        NameLookupResult::Undefined.into(),
    ))
}

/// Every OS call is refused. Monty absorbs `OSError` during glob and walk,
/// so a refused scan raises `RuntimeError` instead and cannot be mistaken for
/// an empty directory; everything else is not handled.
fn os_call_refusal(call_id: u32, function_name: &str) -> pb::ParentRequest {
    if function_name == "Path.scan" {
        return resume_call(
            call_id,
            ExtFunctionResult::Error(MontyException::new(
                ExcType::RuntimeError,
                Some("filesystem access is not available in cells; use the nested tools".into()),
            )),
        );
    }
    resume_call_with(
        call_id,
        pb::ExtFunctionResult {
            kind: Some(pb::ext_function_result::Kind::NotHandled(pb::Unit {})),
        },
        None,
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, reason = "worker unit regressions")]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_declines_results_the_interpreter_never_resumed_on() {
        for outcome in [
            CellOutcome::Completed {
                value: None,
                stored: HashMap::new(),
            },
            CellOutcome::Failed {
                message: "bootstrap failed".into(),
                stored: HashMap::new(),
                generation_lost: true,
            },
        ] {
            // The interpreter waits on call 9, not call 7. Its result can
            // arrive first, but no ResumeFutures hands it to the interpreter.
            let mut worker = Worker {
                child: Child::default(),
                bindings: None,
                running: Some(Running {
                    cell: 1,
                    futures: BTreeSet::from([7, 9]),
                    waiting: Some(vec![9]),
                    ..Running::default()
                }),
            };
            worker
                .nested_result(1, 7, Ok(CellValue::Null))
                .map_err(|lost| lost.message)
                .expect("the result is held, not resumed");
            let mut frames = Vec::new();
            worker
                .finish(outcome.clone(), |frame| {
                    frames.push(frame.clone());
                    Ok(())
                })
                .map_err(|lost| lost.message)
                .expect("terminal");
            assert!(matches!(
                frames.as_slice(),
                [WorkerFrame::Delivery {
                    cell: 1,
                    call: 7,
                    verdict: DeliveryVerdict::NotObserved(_),
                }, WorkerFrame::CellDone { cell: 1, outcome: actual }] if actual == &outcome
            ));
            assert!(worker.running.is_none());
        }
    }

    #[test]
    fn undecodable_traceback_is_a_definite_failure_that_loses_the_generation() {
        let exception = pb::RaisedException {
            exc_type: "RuntimeError".into(),
            message: Some("nested rejection".into()),
            traceback: vec![pb::StackFrame {
                filename: SCRIPT_NAME.into(),
                start: Some(pb::CodeLoc {
                    line: 1,
                    column: 12,
                }),
                end: Some(pb::CodeLoc { line: 1, column: 5 }),
                preview_line: Some("await fetch({})".into()),
                ..pb::StackFrame::default()
            }]
            .into(),
            data: None,
        };
        assert!(
            matches!(exception_outcome(exception), CellOutcome::Failed { generation_lost: true, ref message, .. } if message.starts_with(PLAKOUS_PY_PTC_ERROR_UNDECODABLE_TRACEBACK_PREFIX))
        );
        let ordinary = MontyException::new(ExcType::MemoryError, None);
        assert!(
            matches!(exception_outcome((&ordinary).into()), CellOutcome::Failed { generation_lost: false, ref message, .. } if message.contains("MemoryError"))
        );
    }

    #[test]
    fn cumulative_print_cap_reports_truncation_once_on_a_char_boundary() {
        let mut printed = Printed::default();
        let first = printed
            .admit(&"é".repeat(MAX_PRINT_BYTES))
            .expect("the first print is admitted");
        assert!(first.len() <= MAX_PRINT_BYTES);
        assert!(first.ends_with("[Python output truncated at 1 MiB]\n"));
        let header = "Python cell running with cell ID 18446744073709551615\nOutput:\n";
        assert!(
            header.len() + first.len() <= 1_048_576,
            "the truncation notice must survive status-inclusive projection"
        );
        assert_eq!(printed.admit("more dropped output"), None);
    }

    #[test]
    fn the_session_states_its_limits_and_os_policy_explicitly() {
        let config = session_config();
        let limits = config.limits.expect("resource limits");
        assert_eq!(limits.max_memory_bytes, Some(256 * 1024 * 1024));
        assert_eq!(
            limits.max_suspensions,
            Some(u64::MAX),
            "a finite suspension cap would count across the interpreter's life"
        );
        assert!(!config.type_check);
        assert_eq!(config.script_name, SCRIPT_NAME);
        let policy = config.os_policy.expect("OS policy");
        let expected: pb::OsPolicy = (&OsPolicy {
            datetime: DateTimeSource::System,
            random_start: RandomStart::System,
            sleep: SleepMode::CallHost,
            ..OsPolicy::default()
        })
            .into();
        assert_eq!(policy, expected);
    }

    /// The type checker stays linked for the SDK; the vendored typeshed
    /// must keep checking the built-in modules and refuse unknown imports.
    #[test]
    fn new_builtin_modules_type_check_and_run_against_vendored_stubs() {
        let mut child = Child::default();
        let mut sink = TurnSink::default();
        let configure = pb::Configure {
            type_check: true,
            ..session_config()
        };
        child
            .handle(
                request(pb::parent_request::Kind::Configure(configure)),
                &mut sink,
            )
            .expect("configure");
        assert!(matches!(
            sink.turn_end.take().and_then(|event| event.kind),
            Some(pb::child_event::Kind::Ok(_))
        ));
        let feed = |code: &str| {
            request(pb::parent_request::Kind::Feed(pb::Feed {
                code: code.to_owned(),
                inputs: Vec::new().into(),
                values: None,
                skip_type_check: false,
                cwd: "/".to_owned(),
            }))
        };
        let source = r#"import csv
import hashlib
import tomllib
import bisect
import os.path
assert list(csv.reader(["a,b", "1,2"])) == [["a", "b"], ["1", "2"]]
assert hashlib.sha256(b"x").hexdigest() == "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
assert tomllib.loads("x = 1")["x"] == 1
assert bisect.bisect([1, 3, 5], 4) == 2
assert os.path.join("/a", "b") == "/a/b"
True"#;
        let mut next = feed(source);
        loop {
            child.handle(next, &mut sink).expect("type checked feed");
            match sink.turn_end.take().and_then(|event| event.kind) {
                Some(pb::child_event::Kind::Complete(complete)) => {
                    let value = MontyObject::try_from(complete).expect("value");
                    assert_eq!(format_monty(&value, 1024), "True");
                    break;
                }
                Some(pb::child_event::Kind::OsCall(call)) => {
                    let (call_id, function) = os_call_from_proto(call).expect("os call");
                    assert_eq!(function.name(), "import_stubs");
                    next = os_call_refusal(call_id, function.name());
                }
                other => panic!("unexpected built-in host request: {other:?}"),
            }
        }
        child
            .handle(feed("import plak_host_module"), &mut sink)
            .expect("unknown import requests stubs");
        let Some(pb::child_event::Kind::OsCall(call)) =
            sink.turn_end.take().and_then(|event| event.kind)
        else {
            panic!("expected import_stubs");
        };
        let (call_id, function) = os_call_from_proto(call).expect("os call");
        assert_eq!(function.name(), "import_stubs");
        child
            .handle(os_call_refusal(call_id, function.name()), &mut sink)
            .expect("no host stubs");
        let Some(pb::child_event::Kind::TypingError(typing)) =
            sink.turn_end.take().and_then(|event| event.kind)
        else {
            panic!("an unknown import is a typing error");
        };
        assert!(
            typing.diagnostics.contains("plak_host_module"),
            "{}",
            typing.diagnostics
        );
    }
}
