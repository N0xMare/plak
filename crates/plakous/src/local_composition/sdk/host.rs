//! SDK calls lower only to driver-owned canonical children or named-state events.

use super::{
    refs::Refs,
    values::{self, ValueBudget},
};
use plakous_agent::internal::{
    LocalCompositionEvent as Event, LocalCompositionEventReceiver, LocalCompositionFuture,
    LocalCompositionNestedCall, LocalCompositionNestedDelivery as Delivery,
    LocalCompositionNestedResponder, LocalCompositionSessionFuture, LocalCompositionStateResponder,
    StateError, StateOp, StateResult,
};
use plakous_cell::{CellSessionEvent, CellSessionEvents, CellValue as V};
use plakous_compose::PythonHarnessSdkDescriptorV1 as Limits;
use plakous_tools::{ExecutionValue, ProcessValue};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{mpsc, oneshot};

type Task = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;
type Acks = Arc<Mutex<Vec<oneshot::Sender<Delivery>>>>;

#[derive(Default)]
pub(super) struct Tasks(Mutex<tokio::task::JoinSet<()>>);
impl Tasks {
    fn spawn(&self, task: Task) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .spawn(task);
    }
    fn active(&self) -> bool {
        !self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }
    fn poll(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<(), tokio::task::JoinError>>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .poll_join_next(cx)
    }
    pub(super) async fn stop(&self) -> Result<(), String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .abort_all();
        let mut failure = None;
        while let Some(result) = std::future::poll_fn(|cx| self.poll(cx)).await {
            if let Err(error) = result
                && !error.is_cancelled()
            {
                failure.get_or_insert_with(|| format!("SDK host task failed: {error}"));
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

pub(super) struct Events {
    api: Arc<plakous_compose::HostApi>,
    raw: CellSessionEvents,
    tasks: Arc<Tasks>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    next: Arc<AtomicU64>,
    cell: u64,
    budget: Arc<Mutex<ValueBudget>>,
    refs: Arc<Mutex<Refs>>,
}

impl Events {
    pub(super) fn new(
        raw: CellSessionEvents,
        refs: Arc<Mutex<Refs>>,
        api: Arc<plakous_compose::HostApi>,
        tasks: Arc<Tasks>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(32);
        Self {
            api,
            raw,
            tasks,
            tx,
            rx,
            next: Arc::new(AtomicU64::new(1)),
            cell: 0,
            budget: Arc::default(),
            refs,
        }
    }
}

impl LocalCompositionEventReceiver for Events {
    fn receive(&mut self) -> LocalCompositionSessionFuture<'_, Option<Event>> {
        Box::pin(async move {
            loop {
                tokio::select! {
                    event=self.rx.recv()=>return Ok(event),
                    joined=std::future::poll_fn(|cx|self.tasks.poll(cx)),if self.tasks.active()=> {
                        if let Some(Err(error))=joined {return Err(plakous_agent::internal::LocalCompositionSessionLoss::HostLost(format!("SDK host task failed: {error}")));}
                    },
                    event=self.raw.receive()=>match event {
                        Some(CellSessionEvent::NestedCall{cell,name,input,responder,..})=>{
                            if self.cell!=cell {self.cell=cell;self.budget=Arc::default();}
                            let scope=Scope{api:Arc::clone(&self.api),cell:cell.to_string(),tx:self.tx.clone(),next:Arc::clone(&self.next),acks:Arc::default(),refs:Arc::clone(&self.refs)};
                            let budget=Arc::clone(&self.budget);
                            self.tasks.spawn(Box::pin(async move {
                                let result=execute(&scope,&name,input).await;
                                let value=match result {
                                    Ok(value)=>match budget.lock().unwrap_or_else(std::sync::PoisonError::into_inner).admit(&value) {
                                        Ok(())=>value,
                                        Err(error)=>error_value("quota",error.to_string()),
                                    },
                                    Err(error)=>error,
                                };
                                let delivery=super::super::session::delivery(responder.resolve_cell(value).await);
                                for ack in scope.acks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).drain(..) {let _=ack.send(delivery.clone());}
                            }));
                        }
                        Some(CellSessionEvent::Notification{responder,..})=>{let _=responder.acknowledge();}
                        None=>return Ok(None),
                    }
                }
            }
        })
    }
}

pub(super) fn error_value(kind: &str, message: impl Into<String>) -> V {
    values::record(
        "SdkError",
        [
            ("kind", values::text(kind)),
            ("message", values::text(message)),
        ],
    )
}

struct ToolReply {
    text: Value,
    value: Option<ExecutionValue>,
    ack: oneshot::Sender<Delivery>,
}
struct NestedReply {
    tx: oneshot::Sender<Result<ToolReply, String>>,
    want: bool,
}
impl LocalCompositionNestedResponder for NestedReply {
    fn wants_value(&self) -> bool {
        self.want
    }
    fn resolve(self: Box<Self>, text: Value) -> LocalCompositionFuture<'static, Delivery> {
        self.resolve_value(text, None)
    }
    fn resolve_value(
        self: Box<Self>,
        text: Value,
        value: Option<ExecutionValue>,
    ) -> LocalCompositionFuture<'static, Delivery> {
        Box::pin(async move {
            let (ack, rx) = oneshot::channel();
            if self.tx.send(Ok(ToolReply { text, value, ack })).is_err() {
                return Ok(Delivery::NotObserved(
                    "SDK call retired before receiving its child".into(),
                ));
            }
            Ok(rx.await.unwrap_or(Delivery::HostLost))
        })
    }
    fn reject(self: Box<Self>, reason: String) -> LocalCompositionFuture<'static, Delivery> {
        Box::pin(async move {
            let observed = self.tx.send(Err(reason)).is_ok();
            Ok(if observed {
                Delivery::Accepted
            } else {
                Delivery::NotObserved("SDK call retired".into())
            })
        })
    }
}

struct StateReply(oneshot::Sender<Result<StateResult, StateError>>);
impl LocalCompositionStateResponder for StateReply {
    fn respond(self: Box<Self>, result: Result<StateResult, StateError>) -> bool {
        self.0.send(result).is_ok()
    }
}

struct Scope {
    api: Arc<plakous_compose::HostApi>,
    cell: String,
    tx: mpsc::Sender<Event>,
    next: Arc<AtomicU64>,
    acks: Acks,
    refs: Arc<Mutex<Refs>>,
}
impl Scope {
    async fn call(
        &self,
        tool: &str,
        input: Value,
        want: bool,
    ) -> Result<Option<ExecutionValue>, V> {
        let (tx, rx) = oneshot::channel();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.tx
            .send(Event::NestedCall {
                call: LocalCompositionNestedCall {
                    cell_id: self.cell.clone(),
                    runtime_call_id: format!("sdk_{id}"),
                    tool_name: tool.into(),
                    input: Some(input),
                },
                responder: Box::new(NestedReply { tx, want }),
            })
            .await
            .map_err(|_| error_value("runtime", "SDK driver closed"))?;
        let reply = rx
            .await
            .map_err(|_| error_value("runtime", "SDK child reply closed"))?
            .map_err(|message| error_value("runtime", message))?;
        self.acks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(reply.ack);
        if want && reply.value.is_none() {
            return Err(error_value("runtime", format!("{tool}: {}", reply.text)));
        }
        Ok(reply.value)
    }
    async fn typed(&self, tool: &str, input: Value) -> Result<ExecutionValue, V> {
        self.call(tool, input, true)
            .await?
            .ok_or_else(|| error_value("runtime", "SDK child did not supply a typed value"))
    }
    async fn state(&self, op: StateOp) -> Result<StateResult, V> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Event::State {
                cell_id: self.cell.clone(),
                op,
                responder: Box::new(StateReply(tx)),
            })
            .await
            .map_err(|_| error_value("runtime", "SDK state driver closed"))?;
        rx.await
            .map_err(|_| error_value("runtime", "SDK state reply closed"))?
            .map_err(|error| state_error(&error))
    }
}

fn state_error(error: &StateError) -> V {
    let kind = match error {
        StateError::InvalidKey
        | StateError::InexactNumber
        | StateError::Json(_)
        | StateError::NonCanonical => "value",
        StateError::ValueTooLarge { .. }
        | StateError::TooManyKeys
        | StateError::StateTooLarge { .. } => "quota",
        _ => "runtime",
    };
    error_value(kind, error.to_string())
}

fn validate_state_put(input: &Value) -> Result<(), V> {
    let name = string(input, "name");
    // Measure what get decodes, not the pre-canonicalization number forms.
    let state = plakous_agent::internal::NamedState::default()
        .put(name.into(), &input["value"])
        .map_err(|error| state_error(&error))?;
    let value = state
        .get(name)
        .map_err(|error| state_error(&error))?
        .ok_or_else(|| error_value("runtime", "validated state value is missing"))?;
    values::result_bytes(&V::from(value)).map_err(|error| match error {
        values::ValueQuotaError::Result { bytes } => error_value(
            "quota",
            format!(
                "named-state value encodes to at least {bytes} bytes (maximum {})",
                Limits::RESULT_BYTES
            ),
        ),
        other => error_value("quota", other.to_string()),
    })?;
    Ok(())
}
fn string<'a>(input: &'a Value, key: &str) -> &'a str {
    input[key].as_str().unwrap_or_default()
}
fn optional(input: &Value, key: &str, out: &mut Value, target: &str) {
    if !input[key].is_null() {
        out[target] = input[key].clone();
    }
}
fn timeout(input: &Value) -> Result<u64, V> {
    let seconds = input["timeout_s"].as_f64().unwrap_or(120.0);
    let bound = std::time::Duration::from_millis(Limits::FOREGROUND_MS).as_secs_f64();
    if !seconds.is_finite() || seconds < 0.0 || seconds > bound {
        return Err(error_value(
            "value",
            format!("timeout_s must be within 0..={bound}"),
        ));
    }
    let duration = std::time::Duration::try_from_secs_f64(seconds)
        .map_err(|_| error_value("value", "invalid timeout_s"))?;
    u64::try_from(duration.as_millis())
        .map_err(|_| error_value("value", "timeout_s exceeds milliseconds bound"))
}

async fn execute(scope: &Scope, name: &str, input: Value) -> Result<V, V> {
    scope
        .api
        .validate_python_sdk_call(name, &input)
        .map_err(|message| error_value("value", message))?;
    let converted = match name {
        "ctx.read" => return read(scope, &input).await,
        "ctx.list" => {
            let mut args = json!({"path":input["path"],"depth":input["depth"]});
            optional(&input, "glob", &mut args, "glob");
            scope.typed("list_entries", args).await?
        }
        "ctx.grep" => {
            let mut args = json!({"query":input["pattern"],"path":input["path"],"use_regex":true});
            optional(&input, "glob", &mut args, "filename_glob");
            scope.typed("search_repository", args).await?
        }
        "fs.apply" => {
            scope
                .typed("edit_files", json!({"ops":input["ops"]}))
                .await?
        }
        "plan.update" => {
            let plan = input["items"]
                .as_array()
                .ok_or_else(|| error_value("value", "plan items must be a list"))?
                .iter()
                .map(|row| json!({"step":row["text"],"status":row["status"]}))
                .collect::<Vec<_>>();
            scope.typed("update_plan", json!({"plan":plan})).await?
        }
        "state.get" => {
            return match scope
                .state(StateOp::Get {
                    key: string(&input, "name").into(),
                })
                .await?
            {
                StateResult::Value(value) => Ok(V::from(value.unwrap_or(input["default"].clone()))),
                _ => Err(error_value("runtime", "unexpected state reply")),
            };
        }
        "state.put" => {
            validate_state_put(&input)?;
            scope
                .state(StateOp::Put {
                    key: string(&input, "name").into(),
                    value: input["value"].clone(),
                })
                .await?;
            return Ok(V::Null);
        }
        "state.drop" => {
            return match scope
                .state(StateOp::Drop {
                    key: string(&input, "name").into(),
                })
                .await?
            {
                StateResult::Dropped(existed) => Ok(V::Bool(existed)),
                _ => Err(error_value("runtime", "unexpected state reply")),
            };
        }
        "state.list" => {
            return match scope.state(StateOp::List).await? {
                StateResult::Keys(keys) => Ok(V::List(
                    keys.into_iter()
                        .map(|(key, bytes)| {
                            values::record(
                                "Slot",
                                [
                                    ("name", values::text(key)),
                                    ("bytes", V::Int(i64::try_from(bytes).unwrap_or(i64::MAX))),
                                ],
                            )
                        })
                        .collect(),
                )),
                _ => Err(error_value("runtime", "unexpected state reply")),
            };
        }
        "proc.run" | "proc.shell" | "proc.start" => {
            return start_process(scope, name, &input).await;
        }
        "proc.poll" | "proc.join" | "proc.cancel" | "proc.stdin" => {
            return observe_process(scope, name, &input).await;
        }
        _ => return Err(error_value("value", "unknown SDK function")),
    };
    values::convert(converted).map_err(|message| error_value("runtime", message))
}

async fn read(scope: &Scope, input: &Value) -> Result<V, V> {
    let target = string(input, "target");
    let mut args = json!({"path":target});
    if let Some(lines) = input["lines"].as_array() {
        let range = lines
            .first()
            .and_then(Value::as_u64)
            .zip(lines.get(1).and_then(Value::as_u64))
            .filter(|(first, last)| {
                lines.len() == 2 && *first > 0 && last >= first && last - first < 2000
            });
        let (first, last) = range.ok_or_else(|| {
            error_value("value", "lines must be (first, last), 1-based inclusive")
        })?;
        args["offset"] = json!(first);
        args["limit"] = json!(last - first + 1);
    }
    if let Some(target) = input["target"].get("ref").and_then(Value::as_str) {
        let refs = scope
            .refs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reference = refs
            .get(target)
            .ok_or_else(|| error_value("value", "output reference not available"))?;
        if let super::refs::Reference::JobOutput(path) = reference {
            args["path"] = json!(path);
        } else if let super::refs::Reference::Text(text) = reference {
            let first = args["offset"].as_u64().unwrap_or(1);
            let limit = args["limit"].as_u64().unwrap_or(1000);
            let rows = text
                .lines()
                .skip(usize::try_from(first.saturating_sub(1)).unwrap_or(usize::MAX))
                .take(usize::try_from(limit).unwrap_or(usize::MAX))
                .collect::<Vec<_>>();
            return values::convert(ExecutionValue::FileRead(plakous_tools::FileReadValue {
                path: target.into(),
                text: rows.join("\n"),
                first_line: first,
                lines: rows.len() as u64,
                eof: first.saturating_sub(1) + rows.len() as u64 >= text.lines().count() as u64,
                lines_cut: 0,
                clipped_lines: 0,
                lossy: false,
                revision: None,
            }))
            .map_err(|message| error_value("runtime", message));
        }
    }
    values::convert(scope.typed("read_file", args).await?)
        .map_err(|message| error_value("runtime", message))
}
async fn process(scope: &Scope, tool: &str, args: Value) -> Result<ProcessValue, V> {
    match scope.typed(tool, args).await? {
        ExecutionValue::Process(value) => Ok(value),
        _ => Err(error_value(
            "runtime",
            "process tool returned a foreign value",
        )),
    }
}

fn process_value(scope: &Scope, mut value: ProcessValue) -> V {
    value.full_output = if value.truncated || value.omitted_bytes > 0 {
        value.full_output.and_then(|path| {
            scope
                .refs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .job_output(path)
                .ok()
        })
    } else {
        None
    };
    values::process(value)
}
async fn start_process(scope: &Scope, name: &str, input: &Value) -> Result<V, V> {
    let background = name == "proc.start";
    let deadline = if background {
        None
    } else {
        Some(tokio::time::Instant::now() + std::time::Duration::from_millis(timeout(input)?))
    };
    let command = if name == "proc.run" {
        &input["argv"]
    } else {
        &input["cmd"]
    };
    let pipe = if background {
        input["stdin"].as_bool().unwrap_or(false)
    } else {
        !input["stdin"].is_null()
    };
    let wait = 0;
    let mut args = json!({"workdir":input["cwd"].as_str().unwrap_or("."),"yield_time_ms":wait,"max_output_bytes":40000,"tty":input["tty"].as_bool().unwrap_or(false)});
    let tool = if command.is_array() {
        args["argv"] = command.clone();
        args["stdin"] = json!(pipe);
        "process_session_exec"
    } else {
        args["command"] = command.clone();
        args["login"] = json!(false);
        args["keep_stdin_open"] = json!(pipe);
        "process_session_start"
    };
    let mut value = process(scope, tool, args).await?;
    if background {
        return Ok(values::job(value.job_id));
    }
    if let Some(id) = value.session_id {
        if let Some(stdin) = input["stdin"].as_str() {
            merge_process(&mut value,process(scope,"process_session_write",json!({"session_id":id,"input":stdin,"disposition":"close","yield_time_ms":0,"max_output_bytes":40000})).await?);
        }
        if let Some(deadline) = deadline {
            while value.session_id.is_some() && tokio::time::Instant::now() < deadline {
                let remaining = u64::try_from(
                    deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        .as_millis(),
                )
                .unwrap_or(Limits::FOREGROUND_MS);
                merge_process(
                    &mut value,
                    process(scope, "process_session_poll", poll_args(id, remaining)).await?,
                );
            }
        }
        if value.session_id.is_some() {
            let stopped =
                process(scope, "process_session_terminate", json!({"session_id":id})).await?;
            merge_process(&mut value, stopped);
            value.status = "timed_out".into();
            value.session_id = None;
        }
    }
    Ok(process_value(scope, value))
}

fn merge_process(value: &mut ProcessValue, observed: ProcessValue) {
    value.output.push_str(&observed.output);
    value.status = observed.status;
    value.exit_code = observed.exit_code;
    value.session_id = observed.session_id;
    value.truncated |= observed.truncated;
    value.omitted_bytes = value.omitted_bytes.saturating_add(observed.omitted_bytes);
    value.full_output = observed.full_output.or(value.full_output.take());
}
fn poll_args(id: u64, wait: u64) -> Value {
    json!({"session_id":id,"yield_time_ms":wait,"max_output_bytes":40000,"max_output_records":128,"wait_for_exit":wait>0})
}
async fn observe_process(scope: &Scope, name: &str, input: &Value) -> Result<V, V> {
    let id = string(input, "job")
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| error_value("value", "invalid job id"))?;
    if name == "proc.stdin" {
        scope.call("process_session_write",json!({"session_id":id,"input":input["data"],"disposition":if input["close"]==true {"close"}else{"keep_open"},"yield_time_ms":0,"max_output_bytes":40000}),false).await?;
        return Ok(V::Null);
    }
    let (tool, args) = if name == "proc.cancel" {
        ("process_session_terminate", json!({"session_id":id}))
    } else {
        (
            "process_session_poll",
            poll_args(
                id,
                if name == "proc.join" {
                    timeout(input)?
                } else {
                    0
                },
            ),
        )
    };
    Ok(process_value(scope, process(scope, tool, args).await?))
}
