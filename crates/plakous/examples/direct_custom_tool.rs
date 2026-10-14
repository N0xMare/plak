//! A caller-owned tool exposed directly to a Custom model, in a durable session
//! that survives a simulated restart.
//!
//! Run it with:
//!
//! ```sh
//! cargo run -p plakous --example direct_custom_tool --features sessions-sqlite
//! ```
//!
//! Registering an executor does not expose it: the frozen registry's
//! definitions become the provider configuration's tools. Turn events are
//! streamed for observation only; the `TurnResult` stays terminal authority.
//! The released locator is persisted as text and parsed again by a fresh model
//! and runtime, as a restarted process would.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use plakous::agent::{AgentBuilder, AgentEventKind, TurnResultOutcome};
use plakous::model::{
    ConfiguredLanguageModel, LocalDurabilityDisposition, ToolConcurrency, ToolDefinition,
    ToolEffect, ToolInputContract, ToolName, ToolOutputContract, ToolPresentationMode,
};
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
    OpenSessionRequest, SessionBackend, SessionLocator, SessionOperationId, SessionRuntimeBinding,
};
use plakous::tools::{
    FrozenToolRegistry, RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorBindingIdentity,
    ToolExecutorId, ToolFuture, ToolInvocation, ToolOutcome, ToolRegistration, ToolRegistryBuilder,
    WorkspacePolicy,
};

#[path = "common/scripted.rs"]
mod scripted;

use scripted::{Fallible, ScriptedModel, Turn, Vocabulary};

/// The embedder's own tool, called by the model under its exact name.
#[derive(Default)]
struct Lookup {
    calls: AtomicUsize,
}

impl ToolExecutor for Lookup {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { ToolOutcome::success_text("forty-two") })
    }
}

/// Freezes a registry holding only the caller-owned tool.
fn registry(lookup: Arc<Lookup>) -> Fallible<FrozenToolRegistry> {
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("example.lookup")?,
        ToolExecutorId::new("example.lookup.v1")?,
        ToolExecutorBindingIdentity::new(1, &[b"example.lookup.v1"])?,
        ToolDefinition::new(
            ToolName::new("lookup")?,
            "1.0.0",
            "Look up the embedder's answer for a key.",
            ToolInputContract::structured(serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"key": {"type": "string"}},
                "required": ["key"],
                "additionalProperties": false
            }))?,
            ToolOutputContract::utf8(256)?,
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?,
        lookup,
    ))?;
    Ok(registry.freeze())
}

/// Builds one runtime binding: the registry's definitions are the model's tools.
fn runtime(
    model: &Arc<ScriptedModel>,
    registry: FrozenToolRegistry,
    root: &std::path::Path,
) -> Fallible<SessionRuntimeBinding> {
    let configuration = model.provider_configuration(
        ToolPresentationMode::Direct,
        LocalDurabilityDisposition::DurableRequired,
        registry.definitions(),
    )?;
    let configured =
        ConfiguredLanguageModel::custom(model.clone(), configuration, model.authority.clone())?;
    Ok(SessionRuntimeBinding::new(AgentBuilder::new(
        configured,
        registry,
        Arc::new(WorkspacePolicy::host(root, ".")?),
    )))
}

#[tokio::main]
async fn main() -> Fallible<()> {
    let directory = tempfile::tempdir()?;
    // Stores and workspaces refuse symlinked ancestors, and macOS temp paths
    // start at the /var and /tmp symlinks.
    let root = std::fs::canonicalize(directory.path())?;
    let database = root.join("sessions.sqlite3");
    let lookup = Arc::new(Lookup::default());

    let model = Arc::new(ScriptedModel::new(
        &Vocabulary::direct(),
        vec![
            Turn::Call {
                name: "lookup",
                input: serde_json::json!({"key": "answer"}).to_string(),
            },
            Turn::Text("the answer is forty-two"),
        ],
    )?);
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database)?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("example-direct-create")?,
            runtime(&model, registry(lookup.clone())?, &root)?,
        ))
        .await?
    else {
        return Err("create did not produce a new session".into());
    };

    let mut turn = session.agent().prompt("look up the answer").await?;
    let mut events = turn.take_events()?;
    let observation = tokio::spawn(async move {
        let mut tools_completed = 0_usize;
        // Lag or an observation failure ends observation, never the turn.
        while let Ok(event) = events.recv().await {
            if matches!(event.kind(), AgentEventKind::ToolCompleted { .. }) {
                tools_completed += 1;
            }
        }
        tools_completed
    });
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(lookup.calls.load(Ordering::SeqCst), 1);
    let completed_tools = observation.await?;
    println!("observed {completed_tools} completed tool call(s) in the event stream");

    // Persist the locator as text, then drop every live object.
    let persisted = session.release().await?.to_string();
    drop(sessions);
    drop(model);

    let model = Arc::new(ScriptedModel::new(
        &Vocabulary::direct(),
        vec![Turn::Text("still forty-two")],
    )?);
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database)?)?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("example-direct-reopen")?,
            persisted.parse::<SessionLocator>()?,
            runtime(&model, registry(lookup.clone())?, &root)?,
        ))
        .await?
    else {
        return Err("reopen did not restore a quiescent session".into());
    };
    let result = session
        .agent()
        .prompt("and after the restart?")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    session.release().await?;
    println!("direct custom tool: called once, session reopened from its persisted locator");
    Ok(())
}
