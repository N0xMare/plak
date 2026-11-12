use super::super::{OfflineFace, SELECTOR, hook_environment, selected};
use crate::home::paths::PlakPaths;
use crate::surface_protocol::{HostEvent, UserAction};
use plakous_host_protocol::status::HostPhase;
use plakous_hosted::sessions::projector::{Submission, SubmissionKind};
use plakous_hosted::sessions::{QueueItem, SessionRecord};
use std::path::Path;

const COMMAND: &str = "face-fenced-root";
const INPUT: &str = "fenced face prompt";

#[derive(serde::Serialize)]
struct Receipt<'a> {
    fixture: &'static str,
    session: &'a str,
    with_submission: bool,
    provenance: Provenance,
}

#[derive(serde::Serialize)]
struct Provenance {
    seed: &'static str,
    command_id: &'static str,
    finalized_root: bool,
    reopened: bool,
}

pub(super) fn seed(workspace: &Path, with_submission: bool) -> crate::CliResult<serde_json::Value> {
    let selection = selected().ok_or("fixture requires the offline test model")??;
    if selection.plan.tree || selection.plan.code_mode {
        return Err(crate::CliFailure::new(
            "abandoned-open fixture requires a direct offline script",
        ));
    }
    let home = hook_environment("PLAK_HOME").ok_or("fixture requires explicit PLAK_HOME")?;
    if !Path::new(&home).is_absolute() {
        return Err("fixture home must be absolute".into());
    }
    let workspace = workspace
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let paths = PlakPaths::explicit(home.into()).for_workspace(&workspace);
    if paths.runtime_database().exists() {
        return Err("fixture requires an unused runtime store".into());
    }
    paths.ensure_layout()?;
    let records = std::sync::Arc::new(crate::store::session_records::SqliteSessionStore::new(
        paths.clone(),
    ));
    let roots = crate::commands::support::product_state_roots_from(
        &paths,
        paths.home().join("fixture-state"),
        None,
        records,
        &plakous_hosted::agent::ProcessJobs::default(),
    )?;
    let store = roots.session_records();
    if !store.list_for_workspace(&workspace)?.is_empty() {
        return Err("fixture workspace already has sessions".into());
    }
    let mut record = SessionRecord::new(workspace.clone());
    record.account = Some("codex".into());
    record.model = Some(SELECTOR.into());
    record.effort = Some("medium".into());
    record.composition = crate::product_config::ProductCompositionSelection::Direct;
    let id = record.id.clone();
    let config = selection.host_config(
        OfflineFace::Attached,
        &paths,
        &roots,
        record,
        plakous::agent::ReasoningEffort::Medium,
        true,
        &workspace,
        &plakous_host_protocol::views::ReadyCatalog::default(),
        None,
    )?;
    settle(config)?;
    let mut record = store.load(&id)?;
    let source = record
        .plakous_locator
        .ok_or("seed has no canonical locator")?;
    tokio::runtime::Runtime::new()
        .map_err(|error| error.to_string())?
        .block_on(plakous_offline_crash_harness::seed_fenced_code_cell(
            &paths.runtime_database(),
            source,
            COMMAND,
            INPUT.as_bytes(),
            false,
        ))
        .map_err(|error| error.to_string())?;
    if with_submission {
        let item = QueueItem::new(INPUT);
        record.pending_queue.push(item.clone());
        store.save_submission(
            &mut record,
            &Submission {
                command_id: COMMAND.into(),
                kind: SubmissionKind::RootStart,
                queue_item_id: item.id,
                queue_item_revision: item.revision,
                target_turn_id: None,
                canonical_command: item.text,
                submitted_at_ms: item.submitted_at_ms,
            },
        )?;
    }
    store.save(&mut record)?;
    serde_json::to_value(Receipt {
        fixture: "abandoned-open-v1",
        session: &id,
        with_submission,
        provenance: Provenance {
            seed: "settled-offline-terminal",
            command_id: COMMAND,
            finalized_root: false,
            reopened: false,
        },
    })
    .map_err(|error| crate::CliFailure::new(error.to_string()))
}

fn settle(
    config: plakous_hosted::runtime::HostConfig<crate::surface_protocol::PlakExtension>,
) -> crate::CliResult<()> {
    let (actions, events, worker) = plakous_hosted::runtime::spawn_host_worker(config)?;
    let result = (|| {
        actions
            .send(UserAction::Prompt("seed terminal".into()))
            .map_err(|_| "seed prompt refused")?;
        let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut started = false;
        loop {
            let remaining = until.saturating_duration_since(std::time::Instant::now());
            let event = events
                .recv_timeout(remaining)
                .map_err(|_| "seed did not settle")?;
            started |= matches!(event, HostEvent::PromptStarted(_));
            if started
                && matches!(
                    event,
                    HostEvent::Status {
                        phase: HostPhase::Idle,
                        ..
                    }
                )
            {
                break;
            }
        }
        Ok(())
    })();
    let _ = actions.send(UserAction::Quit);
    crate::commands::support::join_host_worker_bounded(
        worker,
        std::time::Duration::from_secs(30),
        "fixture seed",
    )?;
    result
}
