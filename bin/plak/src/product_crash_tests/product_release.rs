//! Release and reopen derive the current head from the runtime feed.

#![cfg(unix)]
#![forbid(unsafe_code)]

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use crate::product_paths::PlakPaths;
use crate::test_support::{
    ActualHostAction, ActualHostInjection, InjectedRuntimeRole, run_actual_host,
};
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
    OpenSessionRequest, SessionBackend, SessionLocator, SessionOperationId,
};
use plakous_offline_crash_harness::{BoundaryController, runtime_binding};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let root = PlakPaths::isolated_tempdir();
        let home = root.path().join("plak-home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        Ok(Self {
            _root: root,
            home,
            workspace,
        })
    }
}

fn seeded_release_fixture(
    fixture: &Fixture,
) -> TestResult<(String, SessionLocator, BoundaryController)> {
    std::fs::create_dir_all(&fixture.home)?;
    let paths = PlakPaths::explicit(fixture.home.clone());
    paths.ensure_layout().map_err(std::io::Error::other)?;
    let controller = BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        fixture.home.join("release-race-evidence.jsonl"),
    )?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let executor = plakous::application::ApplicationExecutor::new()?;
    let created = executor.block_on(sessions.create(CreateSessionRequest::new(
        SessionOperationId::new("release-race:create")?,
        runtime_binding(&fixture.workspace, controller.clone())?,
    )))??;
    let CreateSessionOutcome::Created { session, .. } = created else {
        return Err("release race runtime session was not newly created".into());
    };
    let locator = executor.block_on(session.release())??;

    let mut record = crate::product_sessions::SessionRecord::new(fixture.workspace.clone());
    record.account = Some("codex".into());
    record.model = Some("offline-loopback-v1".into());
    record.effort = Some("medium".into());
    record.bind_plakous_locator(locator)?;
    crate::product_sessions::save_session(&paths, &mut record)?;
    Ok((record.id, locator, controller))
}

fn release_injection(
    fixture: &Fixture,
    controller: BoundaryController,
) -> TestResult<ActualHostInjection> {
    let target = runtime_binding(&fixture.workspace, controller.clone())?.target_binding();
    let workspace = fixture.workspace.clone();
    Ok(ActualHostInjection::new(
        Arc::new(move |role| {
            assert_eq!(role, InjectedRuntimeRole::Primary);
            runtime_binding(&workspace, controller.clone()).map_err(|error| error.to_string())
        }),
        target,
        Arc::new(|_, _| {}),
    ))
}

#[test]
fn reopening_a_release_projects_the_newer_runtime_head() -> TestResult {
    let fixture = Fixture::new()?;
    let (product_id, source, controller) = seeded_release_fixture(&fixture)?;
    let paths = PlakPaths::explicit(fixture.home.clone());
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let executor = plakous::application::ApplicationExecutor::new()?;
    let opened = executor.block_on(sessions.open(OpenSessionRequest::new(
        SessionOperationId::new("release-source-mismatch:advance")?,
        source,
        runtime_binding(&fixture.workspace, controller.clone())?,
    )))??;
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        return Err("source mismatch runtime did not open".into());
    };
    executor.block_on(async {
        session
            .agent()
            .prompt("advance runtime head")
            .await?
            .result()
            .await
    })??;
    let current = executor.block_on(session.release())??;
    assert_ne!(current, source);

    let injection = release_injection(&fixture, controller)?;
    run_actual_host(
        fixture.home.clone(),
        &product_id,
        injection,
        vec![ActualHostAction::Quit],
    )?;
    let stored = crate::product_sessions::load_session(&paths, &product_id)?;
    assert_eq!(stored.plakous_locator, Some(current));
    Ok(())
}
