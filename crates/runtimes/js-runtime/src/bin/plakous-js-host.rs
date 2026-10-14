//! Process worker for Plak Code Mode (`exec`/`wait`) and js-ptc: JSONL over stdin/stdout.

#![allow(
    missing_docs,
    reason = "binary entrypoint; protocol types are documented in plakous-js-eval"
)]

use std::{
    io::{self, BufRead, Read, Write},
    thread,
};

use plakous_js_eval::{ClientOp, HostOp, NestedDeliveryOutcome, encode_frame, sample_self_rusage};
use plakous_js_runtime::EmbeddedHost;
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

/// A refusal is a per-result answer that leaves the cell undisturbed, and a
/// non-delivery says the interpreter never saw the result at all; only a
/// genuine failure to settle a pending promise is reported as a host loss.
async fn tool_result_frame(
    host: &EmbeddedHost,
    execution_id: u64,
    id: u64,
    value: serde_json::Value,
    success: bool,
) -> HostOp {
    match host
        .deliver_tool_result(execution_id, id, value, success)
        .await
    {
        NestedDeliveryOutcome::Accepted => HostOp::ToolResultAccepted { execution_id, id },
        NestedDeliveryOutcome::Refused(message) => HostOp::ToolResultRefused {
            execution_id,
            id,
            message,
        },
        NestedDeliveryOutcome::NotDelivered(message) => HostOp::ToolResultNotDelivered {
            execution_id,
            id,
            message,
        },
        NestedDeliveryOutcome::Lost(message) => HostOp::HostLost { message },
    }
}

async fn async_run() -> Result<(), String> {
    let host = EmbeddedHost::spawn()?;
    let (cmd_tx, mut cmd_rx) = mpsc::channel(plakous_js_eval::COMMAND_QUEUE_ITEMS);
    thread::Builder::new()
        .name("plakous-js-host-stdin".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut reader = stdin.lock();
            loop {
                let mut line = String::new();
                let Ok(n) = reader
                    .by_ref()
                    .take(8 * 1024 * 1024 + 1)
                    .read_line(&mut line)
                else {
                    break;
                };
                if n == 0 || !line.ends_with('\n') {
                    break;
                }
                if line.is_empty() {
                    continue;
                }
                let Ok(op) = serde_json::from_str::<ClientOp>(&line) else {
                    break;
                };
                // Block rather than end the host. A full queue is back
                // pressure on the reader, not a lost worker: the select loop
                // drains it as soon as the delivery in hand is acknowledged.
                // Only a closed receiver, which means the loop has finished,
                // ends this thread.
                if cmd_tx.blocking_send(op).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| error.to_string())?;
    let mut stdout = io::stdout();
    loop {
        tokio::select! {
            command = cmd_rx.recv() => {
                let Some(command) = command else {
                    break;
                };
                match command {
                    ClientOp::Start {
                        execution_id,
                        source,
                        tools,
                        stored,
                        yield_time_ms,
                    } => host.start_cell(execution_id, &source, stored, tools, yield_time_ms)?,
                    ClientOp::ToolResult {
                        execution_id,
                        id,
                        value,
                        success,
                    } => {
                        let frame = tool_result_frame(&host, execution_id, id, value, success).await;
                        let encoded = encode_frame(&frame)?;
                        stdout.write_all(&encoded).map_err(|error| error.to_string())?;
                        stdout.flush().map_err(|error| error.to_string())?;
                    },
                    ClientOp::Resume {
                        execution_id,
                        yield_time_ms,
                    } => host.resume(execution_id, yield_time_ms)?,
                    ClientOp::Interrupt => host.interrupt(),
                    ClientOp::Shutdown => {
                        let usage = sample_self_rusage();
                        let encoded = encode_frame(&HostOp::ResourceReport {
                            user_cpu_ns: usage.user_cpu_ns,
                            system_cpu_ns: usage.system_cpu_ns,
                            peak_rss_bytes: usage.peak_rss_bytes,
                            rss_bytes: usage.rss_bytes,
                            threads: usage.threads,
                            fds: usage.fds,
                        })?;
                        stdout.write_all(&encoded).map_err(|error| error.to_string())?;
                        stdout.flush().map_err(|error| error.to_string())?;
                        break;
                    }
                }
            }
            event = host.read_event() => {
                match event {
                    Ok(event) => {
                        let encoded = encode_frame(&HostOp::from(event))?;
                        stdout.write_all(&encoded).map_err(|error| error.to_string())?;
                        stdout.flush().map_err(|error| error.to_string())?;
                    }
                    Err(message) => {
                        let encoded = encode_frame(&HostOp::HostLost { message })?;
                        stdout.write_all(&encoded).map_err(|error| error.to_string())?;
                        stdout.flush().map_err(|error| error.to_string())?;
                        break;
                    },
                }
            }
        }
    }
    host.terminate().await?;
    Ok(())
}
