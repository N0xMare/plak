#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::{
    COMPACT_SHRINK_REFUSAL_DIAGNOSTIC, COMPACT_SHRINK_REFUSAL_NOTE, apply_compact_views,
    compact_operator_detail, manual_compact_operator_text,
};
use crate::runtime::COMPACT_FAILED_DETAIL;
use crate::runtime::compact::{compact_answer_is_unadmitted, compact_error_is_unadmitted};
use crate::runtime::test_support::{fresh_injected_attached_host, memory_attached_stub};
use crate::sessions;
use plakous::sessions::{AgentSessionsErrorKind, CompactSessionOutcome, SessionOperationId};

#[test]
fn unknown_without_a_fact_stays_pending_but_unavailable_discards() {
    use sessions::projector::{SessionViews, ViewWrites, WorkflowRequest};
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let source = memory_attached_stub().locator();
    config.session.bind_plakous_locator(source).unwrap();
    config.store.save(&mut config.session).unwrap();
    let request = WorkflowRequest {
        operation_id: "plak:compact:no-fact".into(),
        source_locator: source,
        kind: sessions::SessionWorkflowKind::Compact(sessions::ProductCompactReason::Manual),
    };
    config
        .store
        .submit_workflow_request(&mut config.session, &request)
        .unwrap();
    let views = SessionViews::empty();
    config
        .store
        .commit_projection_views(
            &config.session.id,
            &ViewWrites {
                successors: Vec::new(),
                expected_cursor: plakous::sessions::feed::FeedSeq::ZERO,
                cursor: plakous::sessions::feed::FeedSeq::ZERO,
                upserts: Vec::new(),
                deletes: Vec::new(),
                calls: Vec::new(),
                open_turns: Vec::new(),
                processes: Vec::new(),
            },
            &views.state,
        )
        .unwrap();
    apply_compact_views(
        &mut config,
        None,
        &request.operation_id,
        compact_error_is_unadmitted(AgentSessionsErrorKind::OutcomeUnknown),
        Ok(views.clone()),
    );
    assert_eq!(
        config
            .store
            .load_intents(&config.session.id)
            .unwrap()
            .workflow_requests,
        vec![request.clone()]
    );
    let answer = CompactSessionOutcome::Unavailable {
        operation_id: SessionOperationId::new(&request.operation_id).unwrap(),
        source,
        reason: plakous::sessions::SessionWorkflowUnavailable::Compaction,
    };
    assert!(compact_answer_is_unadmitted(&answer, source));
    apply_compact_views(
        &mut config,
        None,
        &request.operation_id,
        compact_answer_is_unadmitted(&answer, source),
        Ok(views),
    );
    assert_eq!(
        config
            .store
            .load_intents(&config.session.id)
            .unwrap()
            .workflow_requests,
        [] as [WorkflowRequest; 0]
    );
}

#[test]
fn unknown_after_admission_keeps_the_request_and_refresh_debt_is_nonfatal() {
    use sessions::projector::{SessionViews, ViewWrites, WorkflowCompletion, WorkflowRequest};
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let opened = memory_attached_stub();
    let source = opened.locator();
    config.session.bind_plakous_locator(source).unwrap();
    config.store.save(&mut config.session).unwrap();
    let request = WorkflowRequest {
        operation_id: "plak:compact:lost-reply".into(),
        source_locator: source,
        kind: sessions::SessionWorkflowKind::Compact(sessions::ProductCompactReason::Manual),
    };
    config
        .store
        .submit_workflow_request(&mut config.session, &request)
        .unwrap();
    let mut views = SessionViews::empty();
    views.state.runtime_session_id = Some(source.session_id());
    views.state.workflows.insert(
        request.operation_id.clone(),
        WorkflowCompletion {
            admitted_at: Some(1),
            settled: None,
        },
    );
    config
        .store
        .commit_projection_views(
            &config.session.id,
            &ViewWrites {
                successors: Vec::new(),
                expected_cursor: plakous::sessions::feed::FeedSeq::ZERO,
                cursor: plakous::sessions::feed::FeedSeq::ZERO,
                upserts: Vec::new(),
                deletes: Vec::new(),
                calls: Vec::new(),
                open_turns: Vec::new(),
                processes: Vec::new(),
            },
            &views.state,
        )
        .unwrap();
    let unadmitted = compact_error_is_unadmitted(AgentSessionsErrorKind::OutcomeUnknown);
    assert!(!unadmitted);
    let revision = config.session.store_revision;
    apply_compact_views(
        &mut config,
        None,
        &request.operation_id,
        unadmitted,
        Ok(views.clone()),
    );
    assert_eq!(config.session.store_revision, revision);
    assert_eq!(
        config
            .store
            .load_intents(&config.session.id)
            .unwrap()
            .workflow_requests,
        vec![request.clone()]
    );
    // Even a wrong caller classification cannot erase the admitted request.
    apply_compact_views(&mut config, None, &request.operation_id, true, Ok(views));
    apply_compact_views(
        &mut config,
        None,
        &request.operation_id,
        true,
        Err("transient projection fault".into()),
    );
    assert_eq!(config.session.store_revision, revision);
    assert_eq!(
        config
            .store
            .load_intents(&config.session.id)
            .unwrap()
            .workflow_requests,
        vec![request]
    );
    config.store.save(&mut config.session).unwrap();
}

#[test]
fn a_shrink_refusal_is_plain_in_the_transcript_and_kept_in_the_log_detail() {
    let logged = compact_operator_detail(
        COMPACT_FAILED_DETAIL,
        Some(COMPACT_SHRINK_REFUSAL_DIAGNOSTIC),
    );
    assert!(logged.contains(COMPACT_SHRINK_REFUSAL_DIAGNOSTIC));
    assert_eq!(
        manual_compact_operator_text(&logged),
        COMPACT_SHRINK_REFUSAL_NOTE
    );
    assert!(!COMPACT_SHRINK_REFUSAL_NOTE.contains("did not prove"));
    let other = format!("{COMPACT_FAILED_DETAIL}: model refused");
    assert_eq!(manual_compact_operator_text(&other), other);
}
