//! Cell worker for Plak Code Mode (`exec`/`wait`) and js-ptc: the cell
//! protocol over stdin/stdout.

#![allow(
    missing_docs,
    reason = "binary entrypoint; protocol types are documented in plakous-cell"
)]

use std::{
    io::{self, Write},
    thread,
};

use plakous_cell::{
    CELL_PROTOCOL_REVISION, CellOutcome, CellPrelude, CellValue, DeliveryVerdict, EngineConfig,
    HostFrame, LossClass, MAX_CELL_VALUE_DEPTH, MAX_FRAME_BYTES, NestedDeliveryOutcome, OutputItem,
    QUICKJS_WORKER, WorkerFrame, json_depth, read_frame_blocking, sample_self_rusage,
    write_frame_blocking,
};
use plakous_js_runtime::{DEFAULT_YIELD_TIME_MS, EmbeddedHost, RuntimeEvent};
use serde_json::Value;
use tokio::sync::mpsc;

fn main() {
    if let Err(error) = run() {
        eprintln!("plakous-js-host: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async_run())
}

fn write(stdout: &mut impl Write, frame: &WorkerFrame) -> Result<(), String> {
    write_frame_blocking(stdout, frame)
}

/// A refusal is a per-result answer that leaves the cell undisturbed, and a
/// non-delivery says the interpreter never saw the result at all; only a
/// genuine failure to settle a pending promise is reported as a host loss.
#[allow(
    clippy::similar_names,
    reason = "cell and call are the protocol's own field names"
)]
async fn delivery_frame(
    host: &EmbeddedHost,
    cell: u64,
    call: u64,
    outcome: Result<CellValue, String>,
) -> WorkerFrame {
    let (value, success) = match outcome {
        Ok(value) => match Value::try_from(value) {
            Ok(value) => (value, true),
            Err(error) => {
                return WorkerFrame::Delivery {
                    cell,
                    call,
                    verdict: DeliveryVerdict::NotDelivered(error),
                };
            }
        },
        Err(reason) => (Value::String(reason), false),
    };
    let verdict = match host.deliver_tool_result(cell, call, value, success).await {
        NestedDeliveryOutcome::Accepted => DeliveryVerdict::Accepted,
        NestedDeliveryOutcome::Refused(message) => DeliveryVerdict::Refused(message),
        NestedDeliveryOutcome::NotDelivered(message) => DeliveryVerdict::NotDelivered(message),
        NestedDeliveryOutcome::NotObserved(message) => DeliveryVerdict::NotObserved(message),
        NestedDeliveryOutcome::Lost(message) => {
            return WorkerFrame::HostLost {
                class: LossClass::of(&message),
                message,
            };
        }
    };
    WorkerFrame::Delivery {
        cell,
        call,
        verdict,
    }
}

fn output_item(content: &Value) -> OutputItem {
    let field = |name: &str| {
        content
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match content.get("type").and_then(Value::as_str) {
        Some("input_image") => OutputItem::Image {
            url: field("image_url"),
            detail: None,
        },
        Some("input_audio") => OutputItem::Audio {
            url: field("audio_url"),
        },
        _ => OutputItem::Text {
            text: field("text"),
        },
    }
}

fn worker_frame(event: RuntimeEvent) -> WorkerFrame {
    match event {
        RuntimeEvent::ToolCall {
            cell_id,
            id,
            name,
            input,
        } => WorkerFrame::NestedCall {
            cell: cell_id,
            call: id,
            name,
            input: CellValue::from(input),
        },
        RuntimeEvent::Notify { cell_id, text } => WorkerFrame::Output {
            cell: cell_id,
            item: OutputItem::Notify { text },
        },
        RuntimeEvent::Content { cell_id, content } => WorkerFrame::Output {
            cell: cell_id,
            item: output_item(&content),
        },
        RuntimeEvent::Yielded { cell_id } => WorkerFrame::Yielded { cell: cell_id },
        RuntimeEvent::Done { cell_id, stored } => WorkerFrame::CellDone {
            cell: cell_id,
            outcome: CellOutcome::Completed {
                value: None,
                stored,
            },
        },
        RuntimeEvent::Error {
            cell_id,
            message,
            stored,
        } => WorkerFrame::CellDone {
            cell: cell_id,
            outcome: CellOutcome::Failed {
                message,
                stored,
                generation_lost: false,
            },
        },
    }
}

/// Reads `Configure` and checks it names this protocol and these limits.
fn configure(stdin: &mut impl io::Read) -> Result<(), String> {
    let frame = read_frame_blocking(stdin, MAX_FRAME_BYTES)?
        .ok_or_else(|| "parent closed before configure".to_owned())?;
    match serde_json::from_slice::<HostFrame>(&frame) {
        Ok(HostFrame::Configure {
            revision,
            engine,
            sdk_stubs: None,
        }) if revision == CELL_PROTOCOL_REVISION && engine == EngineConfig::QUICKJS => Ok(()),
        Ok(HostFrame::Configure { .. }) => Err("configure names another revision or limits".into()),
        _ => Err("first parent frame is not configure".into()),
    }
}

/// Reads host frames on their own thread. A full queue blocks the reader
/// rather than ending the host: it is back pressure, not a lost worker, and
/// the select loop drains it as soon as the delivery in hand is acknowledged.
/// Only a closed receiver, which means the loop has finished, or the end of
/// the parent's stream ends this thread.
fn spawn_command_reader(commands: mpsc::Sender<HostFrame>) -> Result<(), String> {
    thread::Builder::new()
        .name("plakous-js-host-stdin".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            while let Ok(Some(frame)) = read_frame_blocking(&mut stdin, MAX_FRAME_BYTES) {
                let Ok(frame) = serde_json::from_slice::<HostFrame>(&frame) else {
                    break;
                };
                if commands.blocking_send(frame).is_err() {
                    break;
                }
            }
        })
        .map(drop)
        .map_err(|error| error.to_string())
}

/// What the loop does after one host command or engine event.
enum Step {
    Continue,
    Stop,
}

/// Applies one host command to the engine.
async fn command(
    host: &EmbeddedHost,
    stdout: &mut impl Write,
    command: HostFrame,
) -> Result<Step, String> {
    match command {
        HostFrame::StartCell {
            cell,
            source,
            prelude: CellPrelude::Quickjs { tools, stored },
            window,
        } => host.start_cell(
            cell,
            &source,
            stored,
            tools,
            window.unwrap_or(DEFAULT_YIELD_TIME_MS),
        )?,
        HostFrame::StartCell {
            prelude: CellPrelude::Monty { .. } | CellPrelude::MontySdk { .. },
            ..
        } => {
            write(
                stdout,
                &WorkerFrame::HostLost {
                    class: LossClass::ProtocolViolation,
                    message: "a QuickJS cell needs a QuickJS prelude".into(),
                },
            )?;
            return Ok(Step::Stop);
        }
        HostFrame::NestedResult {
            cell,
            call,
            outcome,
        } => {
            let frame = delivery_frame(host, cell, call, outcome).await;
            write(stdout, &frame)?;
        }
        HostFrame::Resume { cell, window } => host.resume(cell, window)?,
        HostFrame::Cancel { .. } => host.interrupt(),
        HostFrame::Reset => return Err("QuickJS does not accept Python Reset".into()),
        HostFrame::Shutdown => {
            write(
                stdout,
                &WorkerFrame::ResourceReport {
                    usage: sample_self_rusage(),
                },
            )?;
            return Ok(Step::Stop);
        }
        HostFrame::Configure { .. } => {
            let message = "repeated configure".to_owned();
            write(
                stdout,
                &WorkerFrame::HostLost {
                    class: LossClass::ProtocolViolation,
                    message: message.clone(),
                },
            )?;
            return Err(message);
        }
    }
    Ok(Step::Continue)
}

/// Forwards one engine event. A nested call whose input nests too deep for
/// a frame is refused for that one call, inside the cell, rather than
/// losing the worker.
async fn event(
    host: &EmbeddedHost,
    stdout: &mut impl Write,
    event: Result<RuntimeEvent, String>,
) -> Result<Step, String> {
    let lost = match event {
        Ok(RuntimeEvent::ToolCall {
            cell_id, id, input, ..
        }) if json_depth(&input) > MAX_CELL_VALUE_DEPTH => {
            let refusal =
                format!("nested call input nests deeper than {MAX_CELL_VALUE_DEPTH} levels");
            match host
                .deliver_tool_result(cell_id, id, Value::String(refusal), false)
                .await
            {
                NestedDeliveryOutcome::Lost(message) => message,
                _ => return Ok(Step::Continue),
            }
        }
        Ok(event) => {
            write(stdout, &worker_frame(event))?;
            return Ok(Step::Continue);
        }
        Err(message) => message,
    };
    write(
        stdout,
        &WorkerFrame::HostLost {
            class: LossClass::of(&lost),
            message: lost,
        },
    )?;
    Ok(Step::Stop)
}

async fn async_run() -> Result<(), String> {
    let mut stdout = io::stdout();
    write(
        &mut stdout,
        &WorkerFrame::Hello {
            revision: CELL_PROTOCOL_REVISION.to_owned(),
            engine: QUICKJS_WORKER.engine.to_owned(),
            adapter: QUICKJS_WORKER.adapter.to_owned(),
        },
    )?;
    let mut stdin = io::stdin().lock();
    if let Err(message) = configure(&mut stdin) {
        let _ = write(
            &mut stdout,
            &WorkerFrame::HostLost {
                class: LossClass::ProtocolViolation,
                message: message.clone(),
            },
        );
        return Err(message);
    }
    drop(stdin);
    let host = EmbeddedHost::spawn()?;
    let (cmd_tx, mut cmd_rx) = mpsc::channel(plakous_cell::COMMAND_QUEUE_ITEMS);
    spawn_command_reader(cmd_tx)?;
    loop {
        let step = tokio::select! {
            received = cmd_rx.recv() => match received {
                Some(received) => command(&host, &mut stdout, received).await,
                None => Ok(Step::Stop),
            },
            received = host.read_event() => event(&host, &mut stdout, received).await,
        };
        match step {
            Ok(Step::Continue) => {}
            Ok(Step::Stop) => break,
            Err(message) => {
                let _ = host.terminate().await;
                return Err(message);
            }
        }
    }
    host.terminate().await?;
    Ok(())
}
