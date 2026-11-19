use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write as _,
    io,
    process::ExitCode,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use micro_monty::{
    Engine, HostArgs, HostError, HostErrorKind, HostException, HostModule, HostSignature,
    HostValue, Limits, Progress, Resume, Session, TextCut, head_tail,
};
use plakous_cell::{
    CELL_PROTOCOL_REVISION, CellOutcome, CellPrelude, CellValue, DeliveryVerdict, EngineConfig,
    HostFrame, LossClass, MAX_CELL_VALUE_DEPTH, MAX_FRAME_BYTES, MONTY_WORKER, MontyBinding,
    MontyBootstrap, OutputItem, WorkerFrame, read_frame_blocking, sample_self_rusage,
    write_frame_blocking,
};

use crate::{codec, test_hooks::boundary};

// Leave room for the host's status/header and a truncation notice.
const MAX_PRINT_BYTES: usize = 1_048_576 - 64;
const PRINT_NOTICE: &str = "\n[Python output truncated at 1 MiB]\n";
const RESULT_NOTICE: &str = "\n[Python result truncated at 1 MiB]";
const EXCEPTION_REPORT_BYTES: usize = 32 * 1024;
const MAX_OUTSTANDING_NESTED: usize = 16;
const PATH_SCAN: &str = "Path.scan";

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

fn engine_version() -> String {
    let mut version = format!("{}+", micro_monty::VERSION);
    for byte in micro_monty::build_id() {
        let _ = write!(version, "{byte:02x}");
    }
    version
}

pub(crate) fn serve() -> ExitCode {
    let version = engine_version();
    if version != MONTY_WORKER.version {
        eprintln!("plakous-py-host: unexpected micro-monty build: {version}");
        return ExitCode::from(3);
    }
    let hello = WorkerFrame::Hello {
        revision: CELL_PROTOCOL_REVISION.to_owned(),
        engine: micro_monty::ENGINE.to_owned(),
        adapter: MONTY_WORKER.adapter.to_owned(),
    };
    if write(&hello).is_err() {
        return ExitCode::from(3);
    }
    let mut stdin = io::stdin().lock();
    match Worker::configure(&mut stdin).and_then(|mut worker| worker.serve(&mut stdin)) {
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

fn read(stdin: &mut impl io::Read) -> Result<Option<HostFrame>, Lost> {
    read_frame_blocking(stdin, MAX_FRAME_BYTES)
        .map_err(Lost::protocol)?
        .map(|frame| {
            serde_json::from_slice(&frame).map_err(|_| Lost::protocol("malformed host frame"))
        })
        .transpose()
}

struct Worker {
    session: Session,
    ordinal: u64,
    bindings: Option<HashMap<u32, MontyBinding>>,
    running: Option<Running>,
    output: Arc<Mutex<Output>>,
}

#[derive(Default)]
struct Running {
    cell: u64,
    futures: BTreeSet<u64>,
    results: BTreeMap<u64, Result<CellValue, String>>,
    waiting: Vec<u64>,
}

#[derive(Default)]
struct Output {
    cell: Option<u64>,
    bytes: usize,
    truncated: bool,
    accepted: Vec<u64>,
    error: Option<Lost>,
}

impl Output {
    fn accept(&mut self) -> Result<(), Lost> {
        if self.accepted.is_empty() {
            return Ok(());
        }
        let cell = self
            .cell
            .ok_or_else(|| Lost::protocol("acceptance without an active cell"))?;
        for call in self.accepted.drain(..) {
            write(&WorkerFrame::Delivery {
                cell,
                call,
                verdict: DeliveryVerdict::Accepted,
            })?;
        }
        boundary("after_accepted");
        Ok(())
    }

    fn print(&mut self, text: &str) -> Result<(), Lost> {
        // The engine has applied a resume batch before running any output.
        self.accept()?;
        let Some(cell) = self.cell.filter(|_| !self.truncated) else {
            return Ok(());
        };
        let room = (MAX_PRINT_BYTES - PRINT_NOTICE.len()).saturating_sub(self.bytes);
        let end = text.floor_char_boundary(text.len().min(room));
        self.bytes += end;
        let mut text_out = text[..end].to_owned();
        if end < text.len() {
            text_out.push_str(PRINT_NOTICE);
            self.truncated = true;
        }
        if !text_out.is_empty() {
            write(&WorkerFrame::Output {
                cell,
                item: OutputItem::Text { text: text_out },
            })?;
        }
        Ok(())
    }
}

fn output(sink: &Mutex<Output>) -> MutexGuard<'_, Output> {
    sink.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Worker {
    fn configure(stdin: &mut impl io::Read) -> Result<Self, Lost> {
        let memory = match read(stdin)? {
            Some(HostFrame::Configure {
                revision, engine, ..
            }) if revision == CELL_PROTOCOL_REVISION && engine == EngineConfig::MONTY => {
                let EngineConfig::Monty { memory_bytes } = engine else {
                    unreachable!()
                };
                memory_bytes
            }
            Some(HostFrame::Configure { .. }) => {
                return Err(Lost::protocol("configure names another revision or limits"));
            }
            _ => return Err(Lost::protocol("first parent frame is not configure")),
        };
        let engine = Engine::new(micro_monty::EngineConfig::default()).with_limits(
            Limits::default()
                .with_memory(memory)
                .with_recursion_depth(1000),
        );
        let mut session = Session::new(&engine).map_err(|error| Lost::engine(error.to_string()))?;
        session.set_host_args(HostArgs {
            max_bytes: codec::MAX_ARGUMENT_BYTES as u64,
            max_depth: u32::try_from(MAX_CELL_VALUE_DEPTH)
                .map_err(|error| Lost::engine(error.to_string()))?,
            lenient: true,
        });
        session.set_text_limit((MAX_PRINT_BYTES - RESULT_NOTICE.len()) as u64);
        session.set_text_cut(TextCut::HeadTail);
        session.set_int_bits_text(true);
        session.set_wall_clock(Some(Box::new(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |duration| duration.as_secs_f64())
        })));
        let sink = Arc::new(Mutex::new(Output::default()));
        let callback_output = Arc::clone(&sink);
        session.set_output(Box::new(move |text| {
            let mut sink = output(&callback_output);
            if sink.error.is_none() {
                sink.error = sink.print(text).err();
            }
        }));
        Ok(Self {
            session,
            ordinal: 0,
            bindings: None,
            running: None,
            output: sink,
        })
    }

    fn serve(&mut self, stdin: &mut impl io::Read) -> Result<(), Lost> {
        while let Some(frame) = read(stdin)? {
            match frame {
                HostFrame::Reset if self.running.is_none() => {
                    self.session
                        .reset()
                        .map_err(|error| Lost::engine(error.to_string()))?;
                    self.bindings = None;
                }
                HostFrame::Reset => return Err(Lost::protocol("reset while a cell runs")),
                HostFrame::StartCell {
                    cell,
                    source,
                    prelude: CellPrelude::Monty { bootstrap },
                    window: None,
                } => {
                    self.start(cell, &source, bootstrap)?;
                }
                HostFrame::NestedResult {
                    cell,
                    call,
                    outcome,
                } => self.nested_result(cell, call, outcome)?,
                // Host cancellation ends/reaps the worker, including in-flight
                // compute. Receipt of this advisory does not settle the cell.
                HostFrame::Cancel { .. } => {}
                HostFrame::Shutdown => {
                    self.session.cancel();
                    write(&WorkerFrame::ResourceReport {
                        usage: sample_self_rusage(),
                    })?;
                    return Ok(());
                }
                HostFrame::StartCell { .. } | HostFrame::Resume { .. } => {
                    return Err(Lost::protocol(
                        "invalid Python cell prelude or parked window",
                    ));
                }
                HostFrame::Configure { .. } => return Err(Lost::protocol("repeated configure")),
            }
        }
        Ok(())
    }

    fn start(
        &mut self,
        cell: u64,
        source: &str,
        bootstrap: Option<MontyBootstrap>,
    ) -> Result<(), Lost> {
        if self.running.is_some() {
            return Err(Lost::protocol("a cell started while another runs"));
        }
        self.running = Some(Running {
            cell,
            ..Running::default()
        });
        if let Err(message) = self.prepare(bootstrap) {
            return self.fail(message, true);
        }
        *output(&self.output) = Output {
            cell: Some(cell),
            ..Output::default()
        };
        self.ordinal = self
            .ordinal
            .checked_add(1)
            .ok_or_else(|| Lost::engine("Python cell ordinal exhausted"))?;
        let filename = format!("<cell {}>", self.ordinal);
        let code = match self.session.compile(source, &filename) {
            Ok(code) => code,
            Err(error) => return self.fail(bounded_report(error.to_string()), false),
        };
        let step = self.session.start(&code);
        self.drive(step)
    }

    fn prepare(&mut self, bootstrap: Option<MontyBootstrap>) -> Result<(), String> {
        if self.bindings.is_some() {
            return if bootstrap.is_none() {
                Ok(())
            } else {
                Err("Python tool bootstrap repeated".into())
            };
        }
        let MontyBootstrap { source, bindings } = bootstrap.unwrap_or_else(|| MontyBootstrap {
            source: String::new(),
            bindings: Vec::new(),
        });
        if bindings.len() > 64 || source.len() > 65_536 {
            return Err("Python tool bootstrap exceeds its bound".into());
        }
        let mut functions: Vec<_> = bindings
            .iter()
            .map(|binding| HostSignature::any(&binding.function))
            .collect();
        functions.push(HostSignature::any(PATH_SCAN));
        let module = HostModule {
            functions,
            exceptions: vec![HostException {
                name: "ToolError".into(),
                base: HostErrorKind::RuntimeError,
                doc: "A nested tool failed.".into(),
            }],
            ..HostModule::default()
        };
        let functions = self
            .session
            .define(&module)
            .map_err(|error| format!("Python tool bootstrap failed: {error}"))?;
        let code = self
            .session
            .compile(
                &format!("import asyncio\n{source}"),
                "plak-python-prelude.py",
            )
            .map_err(|error| format!("Python tool bootstrap failed: {error}"))?;
        match self.session.start(&code) {
            Progress::Complete(_) => {
                self.bindings = Some(
                    functions
                        .iter()
                        .zip(bindings)
                        .map(|(function, binding)| (function.id, binding))
                        .collect(),
                );
                Ok(())
            }
            Progress::Error(error) => Err(format!(
                "Python tool bootstrap failed: {}",
                bounded_report(error.traceback)
            )),
            _ => Err("Python tool bootstrap unexpectedly suspended".into()),
        }
    }

    fn resume(&mut self, input: Resume) -> Result<Progress, Lost> {
        self.session
            .resume(input)
            .map_err(|error| Lost::engine(format!("micro-monty resume refused: {error}")))
    }

    fn drive(&mut self, mut step: Progress) -> Result<(), Lost> {
        loop {
            {
                let mut sink = output(&self.output);
                if let Some(error) = sink.error.take() {
                    return Err(error);
                }
                if self.session.is_poisoned() {
                    return Err(Lost::engine("micro-monty session is poisoned"));
                }
                sink.accept()?;
            }
            step = match step {
                Progress::HostCall {
                    call,
                    function,
                    name,
                    args,
                    kwargs,
                } => {
                    let answer = self.function_call(call, function, &name, args, kwargs)?;
                    self.resume(answer)?
                }
                Progress::Waiting { calls } => return self.wait_on(calls),
                Progress::Complete(value) => {
                    let value = value.map_or_else(
                        || "None".into(),
                        |mut completion| {
                            if completion.str_truncated {
                                completion.str.push_str(RESULT_NOTICE);
                            }
                            completion.str
                        },
                    );
                    return self.done(CellOutcome::Completed {
                        value: Some(value),
                        stored: HashMap::new(),
                    });
                }
                Progress::Error(error) => return self.fail(bounded_report(error.traceback), false),
                _ => return Err(Lost::engine("unexpected interpreter pause")),
            };
        }
    }

    fn function_call(
        &mut self,
        call: u64,
        function: u32,
        name: &str,
        args: Vec<HostValue>,
        kwargs: Vec<(String, HostValue)>,
    ) -> Result<Resume, Lost> {
        if name == PATH_SCAN {
            return Ok(Resume::Error(HostError::new(
                HostErrorKind::RuntimeError,
                "filesystem access is not available in cells; use the nested tools",
            )));
        }
        let Some(binding) = self
            .bindings
            .as_ref()
            .and_then(|bindings| bindings.get(&function))
        else {
            return Ok(Resume::Error(HostError::new(
                HostErrorKind::NameError,
                "unknown Python host function",
            )));
        };
        let running = self
            .running
            .as_mut()
            .ok_or_else(|| Lost::protocol("a tool call without a running cell"))?;
        if running.futures.len() >= MAX_OUTSTANDING_NESTED {
            return Ok(Resume::Error(HostError::new(
                HostErrorKind::RuntimeError,
                format!(
                    "local composition exceeds its profile-bound limit of {MAX_OUTSTANDING_NESTED} outstanding nested calls"
                ),
            )));
        }
        let input = match codec::arguments(binding.input, args, kwargs) {
            Ok(input) => input,
            Err(error) => return Ok(Resume::Error(error)),
        };
        write(&WorkerFrame::NestedCall {
            cell: running.cell,
            call,
            name: binding.tool.clone(),
            input,
        })?;
        running.futures.insert(call);
        Ok(Resume::Future)
    }

    fn wait_on(&mut self, calls: Vec<u64>) -> Result<(), Lost> {
        let running = self
            .running
            .as_mut()
            .ok_or_else(|| Lost::protocol("a wait without a running cell"))?;
        if calls.iter().any(|call| !running.futures.contains(call)) {
            return Err(Lost::engine("micro-monty requested an unknown nested call"));
        }
        write(&WorkerFrame::Waiting {
            cell: running.cell,
            calls: calls.clone(),
        })?;
        running.waiting = calls;
        Ok(())
    }

    #[allow(
        clippy::similar_names,
        reason = "cell and call are the protocol field names"
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
        if running.cell != cell
            || !running.futures.contains(&call)
            || running.results.contains_key(&call)
        {
            return write(&verdict(DeliveryVerdict::Refused(
                "the result names no pending call of the cell".into(),
            )));
        }
        running.results.insert(call, outcome);
        if !running
            .waiting
            .iter()
            .all(|id| running.results.contains_key(id))
        {
            return Ok(());
        }
        let pending = std::mem::take(&mut running.waiting);
        let mut batch = Vec::with_capacity(pending.len());
        for id in &pending {
            let result = running
                .results
                .remove(id)
                .ok_or_else(|| Lost::protocol("incomplete Python result batch"))?;
            running.futures.remove(id);
            batch.push((*id, codec::outcome(result)));
        }
        output(&self.output).accepted = pending;
        boundary("before_delivery");
        let step = self.resume(Resume::Batch(batch))?;
        self.drive(step)
    }

    fn fail(&mut self, message: String, generation_lost: bool) -> Result<(), Lost> {
        self.done(CellOutcome::Failed {
            message,
            stored: HashMap::new(),
            generation_lost,
        })
    }

    fn done(&mut self, outcome: CellOutcome) -> Result<(), Lost> {
        *output(&self.output) = Output::default();
        let running = self
            .running
            .take()
            .ok_or_else(|| Lost::protocol("a terminal without a running cell"))?;
        for call in running.results.into_keys() {
            write(&WorkerFrame::Delivery {
                cell: running.cell,
                call,
                verdict: DeliveryVerdict::NotObserved(
                    "the cell ended before the interpreter observed the result".into(),
                ),
            })?;
        }
        write(&WorkerFrame::CellDone {
            cell: running.cell,
            outcome,
        })
    }
}

fn bounded_report(report: String) -> String {
    // The engine's text limit also limits successful completions. Keep that
    // budget intact but cap its traceback separately, preserving both ends.
    if report.len() <= EXCEPTION_REPORT_BYTES {
        report
    } else {
        head_tail(&report, EXCEPTION_REPORT_BYTES as u64).0
    }
}
