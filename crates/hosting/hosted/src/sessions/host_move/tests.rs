use super::*;
use crate::sessions::QueueItem;
use crate::sessions::projector::{
    AnchoredIntent, Delivery, LineageStep, RootContinuation, RootContinuationCause,
    RootContinuationDecision, RootStopAnchor, Submission, pending_command_ids,
};
use std::collections::BTreeMap;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn locator(checkpoint: u128) -> TestResult<SessionLocator> {
    Ok(SessionLocator::new(
        uuid::Uuid::from_u128(1).try_into()?,
        uuid::Uuid::from_u128(2).try_into()?,
        uuid::Uuid::from_u128(checkpoint).try_into()?,
    ))
}

fn cut() -> TestResult<(SessionRecord, SessionViews, ProductIntents)> {
    let source = locator(3)?;
    let mut record = SessionRecord::new(std::path::PathBuf::from("/workspace"));
    record.plakous_locator = Some(source);
    record.store_revision = 9;
    record.pending_queue.push(QueueItem {
        id: uuid::Uuid::from_u128(5).to_string(),
        revision: 2,
        text: "owed input".into(),
        submitted_at_ms: 42,
        images: Arc::default(),
    });
    let mut views = SessionViews::empty();
    views.cursor = FeedSeq::new(20);
    views.state.runtime_session_id = Some(source.session_id());
    views.state.lineage.push(LineageStep {
        branch_id: source.branch_id(),
        from_seq: 1,
    });
    views.state.head = Some(FeedLocator {
        session_id: source.session_id(),
        branch_id: source.branch_id(),
        checkpoint_id: source.checkpoint_id(),
    });
    views.state.retained_from = Some(7);
    views
        .state
        .deliveries
        .insert("already-admitted".into(), Delivery::Accepted);
    let submissions = ["already-admitted", "still-owed"]
        .into_iter()
        .map(|id| Submission {
            command_id: id.into(),
            kind: SubmissionKind::RootStart,
            queue_item_id: if id == "already-admitted" {
                uuid::Uuid::from_u128(6).to_string()
            } else {
                record.pending_queue[0].id.clone()
            },
            queue_item_revision: 2,
            target_turn_id: None,
            canonical_command: "{}".into(),
            submitted_at_ms: 42,
        })
        .collect();
    let intents = ProductIntents {
        submissions,
        submission_anchors: BTreeMap::from([
            ("already-admitted".into(), 6),
            ("still-owed".into(), 20),
        ]),
        root_continuations: vec![AnchoredIntent {
            anchor: 20,
            ordinal: 3,
            value: RootContinuation {
                anchor: RootStopAnchor::Turn {
                    turn_id: "stopped-turn".into(),
                },
                host_generation: 2,
                cause: RootContinuationCause::Handoff,
                round: 1,
                decided_at_ms: 43,
                decision: RootContinuationDecision::NewTurn {
                    command_id: "once-only-continuation".into(),
                    withdrawn: vec![],
                },
            },
        }],
        ..ProductIntents::default()
    };
    Ok((record, views, intents))
}

use std::sync::Arc;

#[test]
fn dormant_product_cut_preserves_owed_inputs_and_consumed_decisions_at_the_native_floor()
-> TestResult {
    let (record, views, intents) = cut()?;
    let id = record.id.clone();
    let node = record.node.clone();
    let queue = record.pending_queue.clone();
    let source_intents = intents.clone();
    let target = locator(4)?;
    let seed =
        DormantSession::prepare(record, views, intents, FeedSeq::new(20))?.reconstruct(target)?;
    let (record, mut views, intents) = seed.into_parts();
    assert_eq!(record.id, id);
    assert_eq!(record.node, node);
    assert_eq!(record.pending_queue, queue);
    assert_eq!(record.store_revision, 0);
    assert_eq!(record.plakous_locator, Some(target));
    assert_eq!(record.queue_drain_state, QueueDrainState::RestoredPaused);
    assert_eq!(views.cursor, FeedSeq::new(20));
    assert_eq!(views.state.history_through, Some(20));
    assert_eq!(intents, source_intents);
    views.consume_before_retention(&intents);
    assert_eq!(pending_command_ids(&views, &intents), ["still-owed"]);
    assert_eq!(
        views.state.deliveries.get("already-admitted"),
        Some(&Delivery::Accepted)
    );
    assert!(
        !views
            .state
            .deliveries
            .contains_key("once-only-continuation")
    );
    Ok(())
}

#[test]
fn dormant_product_cut_refuses_incomplete_and_active_sources() -> TestResult {
    for defect in 0..5 {
        let (mut record, mut views, mut intents) = cut()?;
        match defect {
            0 => views.cursor = FeedSeq::new(19),
            1 => views.state.head_working = true,
            2 => record.queue_projection_debt = true,
            3 => intents.submissions[1].kind = SubmissionKind::SendNowSteer,
            _ => {
                intents.submission_anchors.insert("still-owed".into(), 21);
            }
        }
        assert!(DormantSession::prepare(record, views, intents, FeedSeq::new(20)).is_err());
    }
    let (record, views, intents) = cut()?;
    assert!(
        DormantSession::prepare(record, views, intents, FeedSeq::new(20))?
            .reconstruct(locator(3)?)
            .is_err()
    );
    Ok(())
}

fn claimed_cut() -> TestResult<DormantSession> {
    let (record, views, intents) = cut()?;
    let owner = record.id.clone();
    let claim = |identity, kind, binding| MoveIdentityClaim {
        identity,
        kind,
        owner: owner.clone(),
        binding,
        disposition: None,
        outcome: None,
    };
    let mut claims = vec![
        claim(
            record.create_operation_id.clone(),
            MoveIdentityKind::Create,
            owner.clone(),
        ),
        claim(
            record.pending_queue[0].id.clone(),
            MoveIdentityKind::Queue,
            owner.clone(),
        ),
    ];
    claims.extend(intents.submissions.iter().map(|submission| {
        claim(
            submission.binding_identity(),
            MoveIdentityKind::Command,
            submission.binding_digest(&owner),
        )
    }));
    Ok(
        DormantSession::prepare(record, views, intents, FeedSeq::new(20))?
            .with_identity_claims(claims)?
            .reconstruct(locator(4)?)?,
    )
}

#[test]
fn dormant_product_document_preserves_identity_bindings_and_owed_input() -> TestResult {
    let seed = claimed_cut()?;
    let text = serde_json::to_string(&seed.document()?)?;
    let decoded: DormantSessionDocument = serde_json::from_str(&text)?;
    let decoded = decoded.into_cut()?;
    decoded.validate_identity_claims()?;
    assert_eq!(decoded.identity_claims(), seed.identity_claims());
    let (_, mut views, intents) = decoded.into_parts();
    views.state.observe_retained_from(21);
    views.consume_before_retention(&intents);
    assert_eq!(pending_command_ids(&views, &intents), ["still-owed"]);
    assert_eq!(views.cursor, FeedSeq::new(20));
    let mut unknown: serde_json::Value = serde_json::from_str(&text)?;
    unknown["draft_registry"] = serde_json::json!([]);
    assert!(serde_json::from_value::<DormantSessionDocument>(unknown).is_err());
    let mut missing: serde_json::Value = serde_json::from_str(&text)?;
    missing["claims"] = serde_json::json!([]);
    assert!(
        serde_json::from_value::<DormantSessionDocument>(missing)?
            .into_cut()
            .is_err()
    );
    let mut seed = claimed_cut()?;
    seed.claims
        .iter_mut()
        .find(|claim| claim.kind == MoveIdentityKind::Queue)
        .ok_or("queue binding")?
        .disposition = Some("deleted".into());
    assert!(
        seed.document().is_err(),
        "a pending queue item cannot reuse a deleted identity"
    );
    let mut journal: serde_json::Value = serde_json::from_str(&text)?;
    journal["state"]["calls"] = serde_json::json!({});
    assert!(serde_json::from_value::<DormantSessionDocument>(journal).is_err());
    Ok(())
}

#[test]
fn dormant_product_memory_seed_is_once_only_and_survives_projection_cache_loss() -> TestResult {
    use crate::session_store::{MemoryStore, SessionRecordStore};
    let seed = claimed_cut()?.with_plan_authority("retained_fork_namespace".into())?;
    let id = seed.document()?.id().to_owned();
    let target = MemoryStore::new();
    let digest = "a".repeat(64);
    target.install_move_seed("move", &digest, &seed)?;
    assert_eq!(
        target.move_plan_authority(&id)?.as_deref(),
        Some("retained_fork_namespace")
    );
    assert_eq!(target.load_intents(&id)?.submissions.len(), 2);
    let mut record = target.load(&id)?;
    record.title = Some("changed after import".into());
    target.save(&mut record)?;
    target.install_move_seed("move", &digest, &seed)?;
    assert_eq!(target.load(&id)?.title, record.title);
    assert!(target.install_move_seed("other", &digest, &seed).is_err());
    assert!(
        target
            .install_move_seed("move", &"b".repeat(64), &seed)
            .is_err()
    );
    target.delete_views(&id);
    assert!(target.load_views(&id)?.is_none());
    let mut base = target
        .move_projection_base(&id)?
        .ok_or("missing semantic base")?;
    base.state.observe_retained_from(21);
    let intents = target.load_intents(&id)?;
    base.consume_before_retention(&intents);
    assert_eq!(pending_command_ids(&base, &intents), ["still-owed"]);
    let mut claims = target.move_identity_claims(&id)?;
    claims.sort_by(|left, right| left.identity.cmp(&right.identity));
    assert_eq!(claims, seed.identity_claims());
    Ok(())
}
