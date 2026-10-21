//! Read-only recovery verdict and a dry run of the live product projector.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use plakous::sessions::feed::{
    FeedChange, FeedCommandKind, FeedCursor, FeedFact, FeedLocator, FeedPage, FeedPageRequest,
};
use plakous::sessions::rehearsal::{OfflineReopenOutcome, inspect_sqlite};
use plakous::sessions::{AgentSessions, SessionLocator};
use plakous_host_protocol::views::presentation::{
    MAX_PRESENTATION_BYTES, MAX_SNAPSHOT_ROWS, PresentationSnapshot,
};
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::sessions::projector::{self, LineageStep, ProductIntents, SessionViews};
use plakous_recursive_subagents_store::{SqliteTreeStore, TreeInspection};

use crate::home::paths::PlakPaths;

pub(crate) fn rehearse(paths: &PlakPaths, id: &str, output: &mut dyn Write) -> CliResult<()> {
    if !uuid::Uuid::parse_str(id).is_ok_and(|value| value.hyphenated().to_string() == id) {
        return Err(CliFailure::new("rehearsal requires a full session UUID"));
    }
    crate::store::ProductStore::inspect_session_read_only(paths, id).map_err(CliFailure::new)?;
    let (workspace, recorded, intents, views) =
        crate::store::ProductStore::inspect_projection_read_only(paths, id)
            .map_err(CliFailure::new)?;
    let probe_paths = PlakPaths::for_workspace(paths.home().to_path_buf(), &workspace);
    let probe = crate::carry::reload_probe(&probe_paths);
    writeln!(
        output,
        "build_id={} offline reload rehearsal",
        crate::build_identity::binary_id()
    )
    .map_err(stdout_failure)?;
    writeln!(
        output,
        "state_key={} importable_export_versions={:?} config={}",
        probe.state_key,
        probe.export_versions,
        if probe.config_error.is_none() {
            "valid"
        } else {
            "refused"
        }
    )
    .map_err(stdout_failure)?;
    let mut valid = probe.config_error.is_none() && workspace.is_dir();
    let followed = recorded;
    valid &= report_head(paths, "root", followed, views, &intents, output)?;
    let tree = inspect_tree(paths, id)?;
    if let TreeInspection::Tree(tree) = tree {
        for (index, node) in tree
            .rows()
            .nodes()
            .values()
            .filter(|node| node.parent_agent_id.is_some())
            .enumerate()
        {
            // Stored labels, descriptions and paths are not diagnostic output.
            let label = if node.agent_id.split('-').all(|part| {
                part.strip_prefix('c').is_some_and(|number| {
                    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                })
            }) {
                node.agent_id.clone()
            } else {
                valid = false;
                format!("invalid-child-{}", index + 1)
            };
            if node.status == plakous_hosted::agent_tree_protocol::NodeStatus::Closed
                || node.session_locator.is_none()
            {
                writeln!(output, "{label}: skipped:{}", node.status.as_str())
                    .map_err(stdout_failure)?;
                continue;
            }
            if node.active_turn
                && node.status == plakous_hosted::agent_tree_protocol::NodeStatus::Running
            {
                writeln!(output, "{label}: busy: active_turn").map_err(stdout_failure)?;
                valid = false;
                continue;
            }
            let locator = node
                .session_locator
                .as_deref()
                .and_then(|value| value.parse().ok());
            valid &= report_config(paths, &label, &node.path, output)?;
            valid &= report_head(
                paths,
                &label,
                locator,
                None,
                &ProductIntents::default(),
                output,
            )?;
        }
    }
    writeln!(output, "not rehearsed: needs credentials (provider construction, harness compilation and runtime revalidation); no network, model calls, recovery writes or tool dispatch")
        .map_err(stdout_failure)?;
    if valid {
        Ok(())
    } else {
        Err(CliFailure::new("offline reload rehearsal refused"))
    }
}

fn inspect_tree(paths: &PlakPaths, id: &str) -> CliResult<TreeInspection> {
    let mut candidate = id.to_owned();
    let mut seen = BTreeSet::new();
    for _ in 0..64 {
        if !seen.insert(candidate.clone()) {
            return Err(CliFailure::new("rehearsal refused: tree_lineage_integrity"));
        }
        let tree = SqliteTreeStore::inspect(&paths.subagents_database(), &candidate)
            .map_err(|_| CliFailure::new("rehearsal refused: tree_unavailable"))?;
        if !matches!(tree, TreeInspection::TreeAbsent) {
            return Ok(tree);
        }
        let source = crate::store::ProductStore::inspect_tree_source_read_only(paths, &candidate)
            .map_err(|_| CliFailure::new("rehearsal refused: tree_lineage_unavailable"))?;
        let Some(source) = source else {
            return Ok(tree);
        };
        candidate = source;
    }
    Err(CliFailure::new("rehearsal refused: tree_lineage_integrity"))
}

fn report_config(
    paths: &PlakPaths,
    label: &str,
    cwd: &Path,
    output: &mut dyn Write,
) -> CliResult<bool> {
    let paths = PlakPaths::for_workspace(paths.home().to_path_buf(), cwd);
    let valid = cwd.is_dir() && crate::carry::reload_probe(&paths).config_error.is_none();
    writeln!(
        output,
        "{label}: config={}",
        if valid { "valid" } else { "refused" }
    )
    .map_err(stdout_failure)?;
    Ok(valid)
}

fn feed_locator(locator: SessionLocator) -> FeedLocator {
    FeedLocator {
        session_id: locator.session_id(),
        branch_id: locator.branch_id(),
        checkpoint_id: locator.checkpoint_id(),
    }
}

fn report_head(
    paths: &PlakPaths,
    label: &str,
    followed: Option<SessionLocator>,
    views: Option<SessionViews>,
    intents: &ProductIntents,
    output: &mut dyn Write,
) -> CliResult<bool> {
    let Some(followed) = followed else {
        writeln!(output, "{label}: refuses: runtime_unavailable").map_err(stdout_failure)?;
        return Ok(false);
    };
    writeln!(output, "{label}: step=projection").map_err(stdout_failure)?;
    output.flush().map_err(stdout_failure)?;
    let result = dry_run(paths, followed, views, intents);
    let Ok((views, projects, mut laws)) = result else {
        writeln!(output, "{label}: refuses: projection_integrity").map_err(stdout_failure)?;
        return Ok(false);
    };
    let head = plakous_hosted::runtime::projected_head(followed, &views);
    writeln!(output, "{label}: step=runtime_inspection").map_err(stdout_failure)?;
    output.flush().map_err(stdout_failure)?;
    let report = match inspect_sqlite(paths.runtime_database(), head) {
        Ok(report) => report,
        Err(error) => {
            writeln!(output, "{label}: refuses: {:?}", error.kind()).map_err(stdout_failure)?;
            return Ok(false);
        }
    };
    if report.outcome == OfflineReopenOutcome::OpensQuiescent {
        laws &= views.state.open_turns.is_empty();
    }
    writeln!(output, "{label}: offline={:?} recovery_phase={:?} evaluator={} process_budgets={} projects:{projects} laws={}",
        report.outcome, report.recovery_phase,
        if report.evaluator_verified { "verified" } else { "refused: recorded_artifact" },
        if report.job_config_verified { "verified" } else { "refused" },
        if laws { "valid" } else { "refused" }).map_err(stdout_failure)?;
    let rows: Vec<_> = views
        .transcript(intents)
        .into_iter()
        .map(|(_, row)| row)
        .collect();
    let snapshot = plakous_hosted::runtime::rehearse_presentation_snapshot(&rows);
    let presentation_valid = report_snapshot(label, rows.len(), &snapshot, output)?;
    Ok(report.outcome != OfflineReopenOutcome::RefusesRecoveryScope
        && report.evaluator_verified
        && report.job_config_verified
        && laws
        && presentation_valid)
}

/// Reads the retained feed once for lineage and terminal identity validation,
/// then applies only records beyond the stored cursor with the live projector.
fn dry_run(
    paths: &PlakPaths,
    followed: SessionLocator,
    views: Option<SessionViews>,
    intents: &ProductIntents,
) -> CliResult<(SessionViews, usize, bool)> {
    let mut cursor = FeedCursor::start(followed.session_id());
    let mut records = Vec::new();
    let mut behind = None;
    let mut target_tail = None;
    loop {
        match AgentSessions::inspect_feed_sqlite(
            paths.runtime_database(),
            FeedPageRequest::after(cursor),
        )
        .map_err(CliFailure::from)?
        {
            FeedPage::Records {
                records: mut page,
                tail,
                ..
            } => {
                let tail = *target_tail.get_or_insert(tail);
                page.retain(|record| record.header().seq <= tail);
                let Some(last) = page.last() else {
                    break;
                };
                cursor = cursor.advanced_past(last);
                records.extend(page);
                if cursor.applied() >= tail {
                    break;
                }
            }
            FeedPage::BehindRetention {
                retained_from,
                tail,
                heads,
            } => {
                behind = Some((retained_from, tail, heads));
                break;
            }
            _ => return Err(CliFailure::new("feed unavailable")),
        }
    }
    let mut views = match views {
        Some(views) => views,
        None => SessionViews::empty_with_lineage(lineage(&records, followed)?),
    };
    let stored_cursor = views.cursor;
    let mut laws = terminal_law(&records, &views, followed);
    if let Some((retained_from, tail, heads)) = behind {
        if views.cursor.get().saturating_add(1) < retained_from.get() {
            let head = heads
                .iter()
                .find(|head| head.branch_id() == followed.branch_id())
                .copied()
                .ok_or_else(|| CliFailure::new("head unavailable"))?;
            let history = plakous::sessions::rehearsal::inspect_history_tail_sqlite(
                paths.runtime_database(),
                head,
                plakous_hosted::sessions::MAX_TRANSCRIPT_LINES,
                plakous_hosted::sessions::MAX_TRANSCRIPT_BYTES,
            )
            .map_err(CliFailure::from)?;
            let lineage = if views.state.lineage.is_empty() {
                vec![LineageStep {
                    branch_id: head.branch_id(),
                    from_seq: 0,
                }]
            } else {
                views.state.lineage.clone()
            };
            views = projector::rebuild_behind_retention(
                lineage,
                followed.session_id(),
                retained_from,
                tail,
                feed_locator(head),
                &history.iter().map(projector::read_item).collect::<Vec<_>>(),
            );
        } else {
            // The stored cursor is retained even though sequence zero is not.
            records.clear();
            let mut cursor = FeedCursor::new(followed.session_id(), views.cursor);
            loop {
                let FeedPage::Records {
                    records: mut page, ..
                } = AgentSessions::inspect_feed_sqlite(
                    paths.runtime_database(),
                    FeedPageRequest::after(cursor),
                )
                .map_err(CliFailure::from)?
                else {
                    return Err(CliFailure::new("feed retention moved"));
                };
                page.retain(|record| record.header().seq <= tail);
                let Some(last) = page.last() else {
                    break;
                };
                cursor = cursor.advanced_past(last);
                records.extend(page);
                if cursor.applied() >= tail {
                    break;
                }
            }
            laws &= terminal_law(&records, &views, followed);
        }
    }
    let projects = if views.cursor > stored_cursor {
        usize::try_from(views.cursor.get() - stored_cursor.get()).unwrap_or(usize::MAX)
    } else {
        records
            .iter()
            .filter(|record| record.header().seq > stored_cursor)
            .count()
    };
    for page in records.chunks(plakous::sessions::feed::MAX_FEED_PAGE_RECORDS) {
        let mut reads = projector::ReadCache::default();
        for record in page
            .iter()
            .filter(|record| record.header().seq > views.cursor)
        {
            projector::load_reads_sqlite(
                &paths.runtime_database(),
                followed.session_id(),
                projector::plan_reads(record),
                &mut reads,
            )
            .map_err(CliFailure::from)?;
        }
        views = projector::project(&views, intents, page, &reads)
            .map_err(|_| CliFailure::new("projector integrity"))?
            .views;
    }
    let pending = projector::pending_command_ids(&views, intents);
    laws &= pending.iter().all(|id| {
        !matches!(
            views.state.deliveries.get(id),
            Some(projector::Delivery::Accepted)
        )
    });
    Ok((views, projects, laws))
}

fn lineage(
    records: &[plakous::sessions::feed::FeedRecord],
    followed: SessionLocator,
) -> CliResult<Vec<LineageStep>> {
    let mut created = None;
    let mut published = BTreeMap::new();
    for record in records {
        let header = record.header();
        match header.change {
            FeedChange::Created { .. } => {
                created.get_or_insert(header.branch_id);
            }
            FeedChange::Branched { source, .. } => {
                published.insert(header.branch_id, (source.branch_id, header.seq.get()));
            }
            _ => {}
        }
    }
    if created.is_none() {
        return Ok(Vec::new());
    }
    let mut lineage = Vec::new();
    let mut current = followed.branch_id();
    while Some(current) != created {
        let Some((source, seq)) = published.get(&current).copied() else {
            return Err(CliFailure::new("branch publishing record unavailable"));
        };
        lineage.push(LineageStep {
            branch_id: current,
            from_seq: seq,
        });
        if lineage.len() > published.len() {
            return Err(CliFailure::new("cyclic branch lineage"));
        }
        current = source;
    }
    if !lineage.is_empty() {
        lineage.push(LineageStep {
            branch_id: current,
            from_seq: 0,
        });
        lineage.reverse();
    }
    Ok(lineage)
}

fn terminal_law(
    records: &[plakous::sessions::feed::FeedRecord],
    views: &SessionViews,
    followed: SessionLocator,
) -> bool {
    let mut admitted: BTreeSet<String> = views.state.open_turns.keys().cloned().collect();
    let mut terminal = BTreeSet::new();
    for record in records {
        if views
            .state
            .branch_at(record.header().seq.get())
            .or(Some(followed.branch_id()))
            .is_some_and(|branch| branch != record.header().branch_id)
        {
            continue;
        }
        for fact in record.facts() {
            match fact {
                FeedFact::CommandAccepted {
                    kind: FeedCommandKind::Root { .. },
                    turn_id: Some(turn),
                    ..
                } => {
                    admitted.insert(turn.to_string());
                }
                FeedFact::TurnTerminal { turn_id, .. } if !terminal.insert(turn_id.to_string()) => {
                    return false;
                }
                _ => {}
            }
        }
    }
    // Working heads may still have an admitted turn; it is the verdict's
    // closure, not this read-only projection, that publishes its terminal.
    // A retained suffix may start after an admission. Its terminal is not
    // foreign just because the admission is outside the retained window.
    records
        .first()
        .is_none_or(|record| record.header().seq.get() > 1)
        || terminal.iter().all(|turn| admitted.contains(turn))
}

fn report_snapshot(
    label: &str,
    rows: usize,
    snapshot: &PresentationSnapshot,
    output: &mut dyn Write,
) -> CliResult<bool> {
    let validation = snapshot.validate();
    writeln!(
        output,
        "{label}: transcript_rows={rows} projected rows={}/{} byte_limit={} validation={:?}",
        snapshot.rows.len(),
        MAX_SNAPSHOT_ROWS,
        MAX_PRESENTATION_BYTES,
        validation
    )
    .map_err(stdout_failure)?;
    Ok(validation.is_ok())
}

fn stdout_failure(_: std::io::Error) -> CliFailure {
    CliFailure::new("stdout failed")
}

#[cfg(test)]
pub(crate) fn assert_abandonment_rehearsal(
    paths: &PlakPaths,
    record: &crate::store::sessions::SessionRecord,
    head: SessionLocator,
) {
    assert_eq!(
        inspect_sqlite(paths.runtime_database(), head)
            .unwrap()
            .outcome,
        OfflineReopenOutcome::RecoversAbandonedTurn
    );
    assert_finalization_rehearsal(paths, record, head);
}

#[cfg(test)]
pub(crate) fn assert_finalization_rehearsal(
    paths: &PlakPaths,
    record: &crate::store::sessions::SessionRecord,
    head: SessionLocator,
) {
    let before = std::fs::read(paths.product_database()).unwrap();
    let (_, _, intents, views) =
        crate::store::ProductStore::inspect_projection_read_only(paths, &record.id).unwrap();
    assert!(report_head(paths, "root", Some(head), views, &intents, &mut Vec::new()).unwrap());
    assert_eq!(before, std::fs::read(paths.product_database()).unwrap());
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_transcript_projects_a_valid_suffix_without_content_in_report() {
        use plakous_hosted::sessions::{NoticeScope, TranscriptRow};
        let transcript = (0..MAX_SNAPSHOT_ROWS + 100)
            .map(|_| TranscriptRow::error(NoticeScope::Turn, "synthetic-private-text"))
            .collect::<Vec<_>>();
        let snapshot = plakous_hosted::runtime::rehearse_presentation_snapshot(&transcript);
        assert_eq!(snapshot.rows.len(), MAX_SNAPSHOT_ROWS);
        let mut report = Vec::new();
        assert!(report_snapshot("root", transcript.len(), &snapshot, &mut report).unwrap());
        assert!(
            !String::from_utf8(report)
                .unwrap()
                .contains("synthetic-private-text")
        );
    }

    #[test]
    fn invalid_snapshot_refuses_without_echoing_content() {
        let snapshot = PresentationSnapshot {
            schema: u32::MAX,
            ..PresentationSnapshot::default()
        };
        assert!(!report_snapshot("root", 0, &snapshot, &mut Vec::new()).unwrap());
    }

    #[test]
    fn fixture_home_without_session_is_refused_without_state_creation() {
        let home = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(home.path().to_path_buf());
        let mut output = Vec::new();
        assert!(rehearse(&paths, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", &mut output).is_err());
        assert_eq!(output, [] as [u8; 0]);
        assert!(!paths.state_root().exists());
    }

    #[tokio::test]
    async fn fixture_projects_root_and_children_without_writes_or_content()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use plakous::sessions::{
            CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
        };
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use plakous_offline_crash_harness::{BoundaryController, text_runtime_binding};
        let fixture = tempfile::tempdir()?;
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let paths = PlakPaths::explicit(fixture.path().join("home"));
        paths.ensure_layout()?;
        let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            fixture.path().join("evidence"),
        )?;
        let binding = text_runtime_binding(
            &workspace,
            controller,
            "synthetic-private",
            "synthetic-private",
        )?;
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("rehearsal:fixture")?,
                binding,
            ))
            .await?
        else {
            return Err("fixture session was not created".into());
        };
        let mut record = crate::store::sessions::SessionRecord::new(workspace);
        record.plakous_locator = Some(session.locator());

        crate::store::sessions::save_session(&paths, &mut record)?;
        let mut tree = plakous_hosted::topology_runtime::test_support::bootstrap(
            &record.id,
            record.workspace_root.clone(),
        );
        let child = tree
            .spawn_agent(
                "root",
                &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .map_err(|error| error.message)?;
        let child_controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            fixture.path().join("child-evidence"),
        )?;
        let child_binding = text_runtime_binding(
            &record.workspace_root,
            child_controller,
            "synthetic-private-child",
            "synthetic-private-child",
        )?;
        let CreateSessionOutcome::Created {
            session: child_session,
            ..
        } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("rehearsal:child")?,
                child_binding,
            ))
            .await?
        else {
            return Err("fixture child session was not created".into());
        };
        tree.bind_session_locator(&child.agent_id, child_session.locator().to_string())
            .map_err(|error| error.message)?;
        tree.set_node_status_for_test(
            &child.agent_id,
            plakous_hosted::agent_tree_protocol::NodeStatus::RecoveryRequired,
        );
        let pending = tree
            .spawn_agent(
                "root",
                &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .map_err(|error| error.message)?;
        tree.set_node_status_for_test(
            &pending.agent_id,
            plakous_hosted::agent_tree_protocol::NodeStatus::Pending,
        );
        let closed = tree
            .spawn_agent(
                "root",
                &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .map_err(|error| error.message)?;
        tree.bind_session_locator(
            &closed.agent_id,
            record.plakous_locator.unwrap().to_string(),
        )
        .map_err(|error| error.message)?;
        tree.close_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::AttemptScopedRequest {
                agent_id: closed.agent_id.clone(),
                expected_attempt_id: None,
            },
            None,
        )
        .map_err(|error| error.message)?;
        crate::store::tree::save_tree(&paths, &mut tree)?;
        let database = rusqlite::Connection::open(paths.subagents_database())?;
        database.execute(
            "UPDATE agent_nodes SET path=?1 WHERE agent_id=?2",
            rusqlite::params![
                fixture
                    .path()
                    .join("removed-worktree")
                    .display()
                    .to_string(),
                closed.agent_id
            ],
        )?;
        database.execute(
            "UPDATE agent_nodes SET active_turn=1,status=?1 WHERE agent_id=?2",
            rusqlite::params![
                plakous_hosted::agent_tree_protocol::NodeStatus::Running.as_str(),
                child.agent_id
            ],
        )?;
        let mut busy_output = Vec::new();
        assert!(rehearse(&paths, &record.id, &mut busy_output).is_err());
        let busy_text = String::from_utf8(busy_output)?;
        assert!(
            busy_text.contains(&format!("{}: busy: active_turn", child.agent_id)),
            "{busy_text}"
        );
        assert!(!busy_text.contains("synthetic-private"));
        database.execute(
            "UPDATE agent_nodes SET status=?1 WHERE agent_id=?2",
            rusqlite::params![
                plakous_hosted::agent_tree_protocol::NodeStatus::RecoveryRequired.as_str(),
                child.agent_id
            ],
        )?;
        let mut recovering_output = Vec::new();
        rehearse(&paths, &record.id, &mut recovering_output)
            .map_err(|error| error.message().to_owned())?;
        assert!(!String::from_utf8(recovering_output)?.contains("busy: active_turn"));
        database.execute(
            "UPDATE agent_nodes SET active_turn=0 WHERE agent_id=?1",
            [&child.agent_id],
        )?;
        database.execute(
            "UPDATE agent_nodes SET path=?1 WHERE agent_id=?2",
            rusqlite::params![
                fixture.path().join("missing-child").display().to_string(),
                child.agent_id
            ],
        )?;
        let mut broken_output = Vec::new();
        assert!(rehearse(&paths, &record.id, &mut broken_output).is_err());
        let broken_text = String::from_utf8(broken_output)?;
        assert!(broken_text.contains(&format!("{}: config=refused", child.agent_id)));
        assert!(!broken_text.contains("missing-child"));
        database.execute(
            "UPDATE agent_nodes SET path=?1 WHERE agent_id=?2",
            rusqlite::params![
                tree.node(&child.agent_id)
                    .unwrap()
                    .path
                    .display()
                    .to_string(),
                child.agent_id
            ],
        )?;
        drop(database);
        let mut output = Vec::new();
        rehearse(&paths, &record.id, &mut output).map_err(|error| error.message().to_owned())?;
        let text = String::from_utf8(output)?;
        assert!(text.contains("OpensQuiescent"), "{text}");
        assert!(text.contains("not rehearsed: needs credentials"));
        assert!(text.contains(&format!("{}: offline=OpensQuiescent", child.agent_id)));
        assert!(text.contains(&format!("{}: transcript_rows=", child.agent_id)));
        assert!(text.contains(&format!("{}: skipped:pending", pending.agent_id)));
        assert!(text.contains(&format!("{}: skipped:closed", closed.agent_id)));
        assert!(!text.contains("synthetic-private"));
        child_session
            .agent()
            .prompt("synthetic-private-child")
            .await?
            .result()
            .await?;
        let child_current = child_session.release().await?;
        let child_recorded = tree
            .node(&child.agent_id)
            .unwrap()
            .session_locator
            .as_deref()
            .unwrap()
            .parse::<SessionLocator>()?;
        session
            .agent()
            .prompt("synthetic-private")
            .await?
            .result()
            .await?;
        session.release().await?;
        let before_product = std::fs::read(paths.product_database())?;
        let before_runtime = std::fs::read(paths.runtime_database())?;
        let before_tree = std::fs::read(paths.subagents_database())?;
        let mut output = Vec::new();
        rehearse(&paths, &record.id, &mut output).map_err(|error| error.message().to_owned())?;
        let text = String::from_utf8(output)?;
        assert!(text.contains("projects:"));
        assert!(text.contains("root: offline=OpensQuiescent"));
        assert!(text.contains(&format!("{}: offline=OpensQuiescent", child.agent_id)));
        assert!(!text.contains("synthetic-private"));
        assert_eq!(before_product, std::fs::read(paths.product_database())?);
        assert_eq!(before_runtime, std::fs::read(paths.runtime_database())?);
        assert_eq!(before_tree, std::fs::read(paths.subagents_database())?);
        let followed = record.plakous_locator.unwrap();
        let (dry, projects, laws) = dry_run(&paths, followed, None, &ProductIntents::default())
            .map_err(|error| error.message().to_owned())?;
        assert!(projects > 0 && laws);
        let store: std::sync::Arc<dyn plakous_hosted::session_store::SessionRecordStore> =
            std::sync::Arc::new(crate::store::session_records::SqliteSessionStore::new(
                paths.clone(),
            ));
        plakous_hosted::runtime::project_session(&sessions, &store, &record).await?;
        let (_, _, intents, incremental) =
            crate::store::ProductStore::inspect_projection_read_only(&paths, &record.id)?;
        assert_eq!(incremental.as_ref(), Some(&dry));
        let (replayed, projects, laws) = dry_run(&paths, followed, incremental, &intents)
            .map_err(|error| error.message().to_owned())?;
        assert_eq!(projects, 0);
        assert!(laws);
        assert_eq!(replayed, dry);
        let mut database = crate::store::ProductStore::open(&paths)?;
        database.delete_views(&record.id)?;
        drop(database);
        let before = std::fs::read(paths.product_database())?;
        rehearse(&paths, &record.id, &mut Vec::new())
            .map_err(|error| error.message().to_owned())?;
        assert_eq!(before, std::fs::read(paths.product_database())?);
        let FeedPage::Records { mut records, .. } = AgentSessions::inspect_feed_sqlite(
            paths.runtime_database(),
            FeedPageRequest::after(FeedCursor::start(followed.session_id())),
        )?
        else {
            return Err("fixture feed unavailable".into());
        };
        let terminal = records
            .iter()
            .find(|record| {
                record
                    .facts()
                    .iter()
                    .any(|fact| matches!(fact, FeedFact::TurnTerminal { .. }))
            })
            .ok_or("terminal missing")?;
        let mut header = *terminal.header();
        header.seq =
            plakous::sessions::feed::FeedSeq::new(records.last().unwrap().header().seq.get() + 1);
        let duplicate =
            plakous::sessions::feed::FeedRecord::from_store(header, terminal.facts().to_vec());
        records.push(duplicate);
        assert!(!terminal_law(&records, &dry, followed));
        assert_ne!(child_current, child_recorded);
        Ok(())
    }
}
