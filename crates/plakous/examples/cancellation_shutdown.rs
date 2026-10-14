//! Cancelling an in-flight turn, then shutting the agent down.
//!
//! Run it with:
//!
//! ```sh
//! cargo run -p plakous --example cancellation_shutdown
//! ```
//!
//! Cancellation settles the turn as cancelled and leaves the agent usable.
//! Shutdown is terminal: it cancels whatever is in flight, joins it, and
//! refuses every later prompt. Neither is a best-effort signal.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use plakous::agent::{AgentBuilder, TurnResultOutcome};
use plakous::model::{ConfiguredLanguageModel, LocalDurabilityDisposition, ToolPresentationMode};
use plakous::tools::{ToolRegistryBuilder, WorkspacePolicy};

#[path = "common/scripted.rs"]
mod scripted;

use scripted::{Fallible, ScriptedModel, Turn, Vocabulary};

#[tokio::main]
async fn main() -> Fallible<()> {
    let directory = tempfile::tempdir()?;
    // Stores and workspaces refuse symlinked ancestors, and macOS temp paths
    // start at the /var and /tmp symlinks.
    let root = std::fs::canonicalize(directory.path())?;
    let model = Arc::new(ScriptedModel::new(
        &Vocabulary::direct(),
        vec![
            Turn::AwaitCancellation,
            Turn::Text("settled after cancellation"),
            Turn::AwaitCancellation,
        ],
    )?);
    let configuration = model.provider_configuration(
        ToolPresentationMode::Direct,
        LocalDurabilityDisposition::Ephemeral,
        &[],
    )?;
    let configured =
        ConfiguredLanguageModel::custom(model.clone(), configuration, model.authority.clone())?;
    let agent = AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
        Arc::new(WorkspacePolicy::host(&root, ".")?),
    )
    .build()?;

    // Cancelling one turn settles it as cancelled and leaves the agent live.
    let turn = agent.prompt("start a long turn").await?;
    model.parked.notified().await;
    turn.control().cancel().await?;
    assert!(matches!(
        turn.result().await?.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    assert!(matches!(
        agent
            .prompt("still usable")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));

    // Shutdown is terminal: it cancels the active turn, joins it, and refuses
    // every later prompt.
    let active = agent.prompt("start another long turn").await?;
    model.parked.notified().await;
    agent.shutdown().await?;
    assert!(matches!(
        active.result().await?.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    assert!(agent.prompt("after shutdown").await.is_err());
    assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    println!("cancellation settled one turn; shutdown joined the next and closed the agent");
    Ok(())
}
