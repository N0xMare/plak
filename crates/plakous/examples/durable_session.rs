//! Creating a durable session, releasing it, and reopening it exactly.
//!
//! Run it with:
//!
//! ```sh
//! cargo run -p plakous --example durable_session --features sessions-sqlite
//! ```
//!
//! A released session is reopened by its locator, not recreated. The reopened
//! conversation is the same canonical history, byte for byte, and the agent
//! continues from it.

use std::sync::Arc;

use plakous::agent::{AgentBuilder, TurnResultOutcome};
use plakous::model::{ConfiguredLanguageModel, LocalDurabilityDisposition, ToolPresentationMode};
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
    OpenSessionRequest, SessionBackend, SessionOperationId, SessionRuntimeBinding,
};
use plakous::tools::{ToolRegistryBuilder, WorkspacePolicy};

#[path = "common/scripted.rs"]
mod scripted;

use scripted::{Fallible, ScriptedModel, Turn, Vocabulary};

/// Builds one runtime binding over a fresh scripted model.
fn runtime(model: &Arc<ScriptedModel>, root: &std::path::Path) -> Fallible<SessionRuntimeBinding> {
    let configuration = model.provider_configuration(
        ToolPresentationMode::Direct,
        LocalDurabilityDisposition::DurableRequired,
        &[],
    )?;
    let configured =
        ConfiguredLanguageModel::custom(model.clone(), configuration, model.authority.clone())?;
    Ok(SessionRuntimeBinding::new(AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
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
    let model = Arc::new(ScriptedModel::new(
        &Vocabulary::direct(),
        vec![Turn::Text("first answer"), Turn::Text("second answer")],
    )?);

    let sessions = AgentSessions::new(SessionBackend::sqlite(&database)?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("example-create")?,
            runtime(&model, &root)?,
        ))
        .await?
    else {
        return Err("create did not produce a new session".into());
    };
    assert!(matches!(
        session
            .agent()
            .prompt("first question")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let before = serde_json::to_string(
        &session
            .agent()
            .conversation()
            .items()
            .iter()
            .collect::<Vec<_>>(),
    )?;

    // The locator is the only handle the reopen needs.
    let locator = session.release().await?;
    drop(sessions);

    let sessions = AgentSessions::new(SessionBackend::sqlite(&database)?)?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("example-reopen")?,
            locator,
            runtime(&model, &root)?,
        ))
        .await?
    else {
        return Err("reopen did not restore a quiescent session".into());
    };
    let after = serde_json::to_string(
        &session
            .agent()
            .conversation()
            .items()
            .iter()
            .collect::<Vec<_>>(),
    )?;
    assert_eq!(before, after, "the reopened history must be identical");
    assert!(matches!(
        session
            .agent()
            .prompt("second question")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    session.release().await?;
    println!("durable session: created, released, reopened by locator with identical history");
    Ok(())
}
