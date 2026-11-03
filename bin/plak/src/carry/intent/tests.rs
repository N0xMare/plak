//! Settlement of interrupted carries: owned by the source branch's lease,
//! re-checked after the head moved, and withdrawn when the source is gone.

use super::*;
use crate::product_paths::PlakPaths;
use crate::shell::services::Services;
use plakous::sessions::{CreateSessionOutcome, CreateSessionRequest};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Fixture {
    _root: tempfile::TempDir,
    home: std::path::PathBuf,
    workspace: std::path::PathBuf,
    services: Services,
    source: SessionRecord,
}

fn binding(fixture_root: &std::path::Path) -> TestResult<plakous::sessions::SessionRuntimeBinding> {
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        fixture_root.join("evidence.jsonl"),
    )?;
    plakous_offline_crash_harness::runtime_binding(fixture_root, controller)
        .map_err(|error| -> Box<dyn std::error::Error> { error })
}

fn block_on<F: Future>(future: F) -> TestResult<F::Output> {
    Ok(plakous::application::ApplicationExecutor::new()?.block_on(future)?)
}

/// Creates and releases one session on one executor, so its driver lives
/// until the release.
async fn create_released(
    sessions: &AgentSessions,
    request: CreateSessionRequest,
) -> Result<SessionLocator, String> {
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(request)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Err("the session was not created".to_owned());
    };
    session.release().await.map_err(|error| error.to_string())
}

impl Fixture {
    /// A source session with a product record, and an intent record that
    /// carries it under an unpublished create.
    fn new() -> TestResult<(Self, SessionRecord)> {
        let root = PlakPaths::isolated_tempdir();
        let home = root.path().join("plak-home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace)?;
        let paths = PlakPaths::explicit(home.clone());
        paths.ensure_layout().map_err(std::io::Error::other)?;
        let services = Services::for_home(paths);
        let sessions = services.state_roots()?.runtime_sessions()?;
        let runtime = binding(&workspace)?;
        let locator = block_on(create_released(
            &sessions,
            CreateSessionRequest::new(SessionOperationId::new("settle:source")?, runtime),
        ))??;
        let mut journal =
            crate::test_support::ProductJournal::create(home.clone(), workspace.clone())?;
        journal.bind_locator(locator)?;
        let source = services
            .session_records()?
            .load(journal.product_session_id())?;
        let mut intent = SessionRecord::new(workspace.clone());
        intent.link_carried_forward(&source.id, &crate::home::state::state_key());
        intent.carry_intent = Some(plakous_hosted::sessions::CarryIntent {
            source: locator,
            continues_line: true,
            cause: "carried".into(),
            orchestration: false,
        });
        services
            .session_records()?
            .save(&mut intent)
            .map_err(|error| error.to_string())?;
        Ok((
            Self {
                _root: root,
                home,
                workspace,
                services,
                source,
            },
            intent,
        ))
    }

    fn sessions(&self) -> TestResult<std::sync::Arc<AgentSessions>> {
        Ok(self.services.state_roots()?.runtime_sessions()?)
    }

    fn source_locator(&self) -> TestResult<SessionLocator> {
        Ok(self.source.plakous_locator.ok_or("source unbound")?)
    }

    /// Another carry moves the source's line to a session no record names.
    fn move_line_elsewhere(&self) -> TestResult {
        let request = CreateSessionRequest::new(
            SessionOperationId::new("settle:elsewhere")?,
            binding(&self.workspace)?,
        )
        .continuing_line(self.source_locator()?);
        let sessions = self.sessions()?;
        block_on(create_released(&sessions, request))??;
        Ok(())
    }

    fn intent_exists(&self, intent: &SessionRecord) -> TestResult<bool> {
        Ok(self.services.session_records()?.load(&intent.id).is_ok())
    }
}

/// An intent whose source's line moved elsewhere is withdrawn, and the open
/// lands on the source.
#[test]
fn an_intent_whose_source_line_moved_elsewhere_is_withdrawn() -> TestResult {
    let (fixture, intent) = Fixture::new()?;
    fixture.move_line_elsewhere()?;
    let Settled::Withdrawn(Some(source), note) = settle(&fixture.services, intent.clone())? else {
        return Err("the intent was not withdrawn".into());
    };
    assert_eq!(source.id, fixture.source.id);
    assert!(note.contains("no longer its line's head"), "{note}");
    assert!(!fixture.intent_exists(&intent)?);
    Ok(())
}

/// A live owner of the source (a carry in another face) leaves the intent
/// pending, untouched, with the lease period to wait.
#[test]
fn an_intent_whose_source_is_held_stays_pending() -> TestResult {
    let (fixture, intent) = Fixture::new()?;
    fixture.move_line_elsewhere()?;
    let sessions = fixture.sessions()?;
    let hold = block_on(sessions.hold_branch(
        &SessionOperationId::new("settle:live-carry")?,
        fixture.source_locator()?,
    ))??;
    let Settled::Pending(note) = settle(&fixture.services, intent.clone())? else {
        return Err("a held source was settled".into());
    };
    assert!(
        note.contains(&format!("retry in {} s", sessions.lease_period_seconds())),
        "{note}"
    );
    assert!(fixture.intent_exists(&intent)?);
    let refused = resolve_open(
        &fixture.services,
        fixture.services.session_records()?.load(&intent.id)?,
    )
    .err()
    .ok_or("a pending carry opened")?;
    assert!(
        refused.message().contains("pending"),
        "{}",
        refused.message()
    );
    block_on(hold.release())??;
    Ok(())
}

/// A carry that publishes after the head check is seen by the second
/// published check: the intent binds instead of being withdrawn. A session
/// under the carry's operation that is not a carry of its source is refused
/// as an integrity failure and the record is kept.
#[test]
fn a_carry_published_after_the_head_check_binds_and_a_foreign_one_is_refused() -> TestResult {
    for carry in [true, false] {
        let (fixture, intent) = Fixture::new()?;
        fixture.move_line_elsewhere()?;
        let operation = SessionOperationId::new(&intent.create_operation_id)?;
        let sessions = fixture.sessions()?;
        let workspace = fixture.workspace.clone();
        let source = fixture.source_locator()?;
        let settled = settle_with(&fixture.services, intent.clone(), &|| {
            let request = CreateSessionRequest::new(
                operation.clone(),
                binding(&workspace).map_err(|error| CliFailure::new(error.to_string()))?,
            );
            // The source is superseded: its carry starts a new line that
            // names it, and needs no lease of it.
            let request = if carry {
                request.continuing_line(source)
            } else {
                request
            };
            block_on(create_released(&sessions, request))
                .map_err(|error| CliFailure::new(error.to_string()))?
                .map_err(CliFailure::new)?;
            Ok(())
        });
        if carry {
            let Settled::Landed(record, _) = settled? else {
                return Err("the raced carry was withdrawn".into());
            };
            assert_eq!(record.id, intent.id);
            let genesis = AgentSessions::genesis_locator_for(&operation)?;
            assert_eq!(record.plakous_locator, Some(genesis));
            assert!(record.carry_intent.is_none());
        } else {
            let refused = settled.err().ok_or("a foreign session bound as a carry")?;
            assert!(
                refused.message().contains("not a carry of its source"),
                "{}",
                refused.message()
            );
            let kept = fixture.services.session_records()?.load(&intent.id)?;
            assert!(kept.carry_intent.is_some() && kept.plakous_locator.is_none());
        }
    }
    Ok(())
}

/// A settlement that fails after taking the source's lease releases it: the
/// source is not left leased for a lease period.
#[test]
fn a_failed_settlement_releases_the_source() -> TestResult {
    let (fixture, intent) = Fixture::new()?;
    fixture.move_line_elsewhere()?;
    let failed = settle_with(&fixture.services, intent.clone(), &|| {
        Err(CliFailure::new("injected settlement failure"))
    });
    assert!(failed.is_err());
    assert!(fixture.intent_exists(&intent)?);
    let hold = block_on(fixture.sessions()?.hold_branch(
        &SessionOperationId::new("settle:after-failure")?,
        fixture.source_locator()?,
    ))?
    .map_err(|error| format!("the failed settlement left its source held: {error}"))?;
    block_on(hold.release())??;
    Ok(())
}

/// An intent whose source record was deleted, or whose source runtime was
/// archived, is withdrawn with a note naming why.
#[test]
fn an_intent_whose_source_is_gone_is_withdrawn() -> TestResult {
    let (fixture, intent) = Fixture::new()?;
    let mut product = crate::store::ProductStore::open(&PlakPaths::explicit(fixture.home.clone()))
        .map_err(|error| error.to_string())?;
    product.delete_session(&fixture.source.id, fixture.source.store_revision)?;
    let Settled::Withdrawn(None, note) = settle(&fixture.services, intent.clone())? else {
        return Err("an intent without a source was kept".into());
    };
    assert!(note.contains("no longer exists"), "{note}");
    assert!(!fixture.intent_exists(&intent)?);

    let (fixture, intent) = Fixture::new()?;
    block_on(fixture.sessions()?.archive(fixture.source_locator()?))??;
    let Settled::Withdrawn(Some(_), note) = settle(&fixture.services, intent.clone())? else {
        return Err("an intent over an archived source was kept".into());
    };
    assert!(note.contains("archived or deleted"), "{note}");
    assert!(!fixture.intent_exists(&intent)?);
    Ok(())
}

/// Deleting a source whose carry is still an intent is refused.
#[test]
fn deleting_the_source_of_a_pending_carry_is_refused() -> TestResult {
    let (fixture, intent) = Fixture::new()?;
    let refused =
        crate::commands::session::refuse_stranding_a_carry(&fixture.services, &fixture.source)
            .err()
            .ok_or("the source of a pending carry was deleted")?;
    assert!(
        refused
            .message()
            .contains(&format!("interrupted carry into session {}", intent.id)),
        "{}",
        refused.message()
    );
    assert!(fixture.intent_exists(&intent)?);
    Ok(())
}
