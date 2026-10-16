//! Offline process-crash evidence utility for the public Plakous facade.

#![forbid(unsafe_code)]

/// The shared runtime/product/tree commit tracer; no supported facade API.
pub use plakous_store::commit_trace;

use std::io::{self, BufRead, BufReader};

mod artifact_fixture;
mod crossed_root;
/// Fsynced physical provider/effect evidence records.
pub mod evidence;
mod finalization_debt;
mod fixture;
/// Explicit in-process and cross-process scripted provider gates.
pub mod gate;
/// Read-only decoded ledger facts for crash-sweep tests.
pub mod ledger_signature;
/// Versioned parent/child control protocol.
pub mod protocol;
/// Declarative offline model scripts.
pub mod script;

pub use crossed_root::seed_crossed_root;
pub use finalization_debt::{
    seed_fenced_code_cell, seed_finalization_debt, seed_uncertain_terminal_siblings,
};

pub use fixture::{
    BoundaryController, compaction_runtime_binding,
    compaction_runtime_binding_with_projected_input,
    compaction_runtime_binding_with_projected_input_and_identity_namespace,
    construction_refused_runtime_binding, exited_child_process_runtime_binding,
    failing_effect_runtime_binding, overflow_runtime_binding, placeholder_runtime_binding,
    projected_terminal_failure_runtime_binding, runtime_binding, runtime_binding_for_account,
    runtime_binding_for_account_with_identity_namespace, scripted_artifact_runtime_binding,
    scripted_code_mode_runtime_binding, scripted_code_mode_runtime_binding_with_extensions,
    scripted_compaction_runtime_binding, scripted_pi_runtime_binding, scripted_runtime_binding,
    scripted_runtime_binding_with_cursor, scripted_runtime_binding_with_extensions,
    scripted_topology_runtime_binding, scripted_topology_runtime_binding_with_clock,
    terminal_failure_runtime_binding, text_blocks_runtime_binding, text_runtime_binding,
    text_runtime_binding_with_finish, topology_runtime_binding,
};
use fixture::{probe_fixture, run_fixture, write_reply};
use protocol::{ChildReply, ControlCommand, PROTOCOL_VERSION};

/// Reads and executes one versioned JSON-lines command from standard input.
///
/// This is public only so the package's thin binary can share the tested child
/// implementation; it is not a Plakous API.
#[allow(
    clippy::too_many_lines,
    reason = "the versioned child protocol is one exhaustive command dispatch boundary"
)]
pub async fn child_main() -> i32 {
    match read_command().and_then(|command| {
        command.validate_version()?;
        Ok(command)
    }) {
        Ok(command) => {
            if write_reply(&ChildReply::Started {
                version: PROTOCOL_VERSION,
            })
            .is_err()
            {
                return 2;
            }
            let result = match command {
                ControlCommand::Run {
                    database,
                    workspace,
                    evidence,
                    state,
                    cut,
                    ..
                } => Box::pin(run_fixture(&database, &workspace, &evidence, &state, cut))
                    .await
                    .map(|locator| ChildReply::Finished {
                        version: PROTOCOL_VERSION,
                        locator,
                    }),
                ControlCommand::Probe {
                    database,
                    workspace,
                    evidence,
                    state,
                    ..
                } => Box::pin(probe_fixture(&database, &workspace, &evidence, &state))
                    .await
                    .map(|report| ChildReply::Probed {
                        version: PROTOCOL_VERSION,
                        report,
                    }),
            };
            match result {
                Ok(reply) => match write_reply(&reply) {
                    Ok(()) => 0,
                    Err(_) => 2,
                },
                Err(error) => {
                    let reply = ChildReply::Failed {
                        version: PROTOCOL_VERSION,
                        message: bounded_message(&error.to_string()),
                    };
                    let _ = write_reply(&reply);
                    1
                }
            }
        }
        Err(message) => {
            let reply = ChildReply::Failed {
                version: PROTOCOL_VERSION,
                message: bounded_message(message),
            };
            let _ = write_reply(&reply);
            2
        }
    }
}

fn read_command() -> Result<ControlCommand, &'static str> {
    let mut line = String::new();
    BufReader::new(io::stdin())
        .read_line(&mut line)
        .map_err(|_| "control input could not be read")?;
    if line.is_empty() {
        return Err("control input ended before one command");
    }
    serde_json::from_str(&line).map_err(|_| "control input was not valid versioned JSON")
}

fn bounded_message(message: &str) -> String {
    message.chars().take(512).collect()
}
