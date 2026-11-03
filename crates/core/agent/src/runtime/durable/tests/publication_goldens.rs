//! Original-code pins for the recovery publication boundary and root closure.

use super::*;
use crate::runtime::control::{command_binding, command_canonical_bytes};
use crate::runtime::durable::RecoveryPublicationOwner;

fn id<T: TryFrom<uuid::Uuid>>(value: u128) -> TestResult<T>
where
    T::Error: std::error::Error + 'static,
{
    Ok(T::try_from(uuid::Uuid::from_u128(value))?)
}

fn source(root: bool) -> TestResult<RuntimeCheckpoint> {
    if !root {
        let base = crate::checkpoint_bundle::tests::fixture_with_history(
            CanonicalHistory::default(),
            0,
            false,
        )?;
        let mut operations = base
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        operations.push(DurableOperationRecord::new(
            2,
            DurableOperationKind::Effect {
                turn_id: id(7_001)?,
                attempt_id: id(7_002)?,
                call_id: id(7_003)?,
                source_index: 0,
            },
            runtime_component(RuntimeComponentKind::Operation, b"golden request".to_vec())?,
            vec![DurableOperationTransition::new(
                base.id(),
                DurableOperationState::Prepared,
            )],
        )?);
        let ledger = RuntimeOperationLedger::new(
            base.manifest()
                .ledger()
                .commands()
                .into_iter()
                .cloned()
                .collect(),
            operations,
        )?;
        let lifecycle = working_lifecycle(id(7_001)?, &ledger)?;
        return crate::checkpoint_bundle::tests::fixture_with_history_and_ledger(
            CanonicalHistory::default(),
            0,
            true,
            Some(ledger),
            Some(lifecycle),
        );
    }
    let turn_id = id::<TurnId>(7_001)?;
    let command = TurnCommand::Start {
        command_id: CommandId::new("golden:root")?,
        input: "recorded input".into(),
    };
    let envelope = DurableCommandEnvelope {
        version: 1,
        canonical_input: command_canonical_bytes(&command),
        effect: DurableActorCommandEffect::Root,
        rejection: None,
    };
    let component = runtime_component(
        RuntimeComponentKind::Command,
        serde_json::to_vec(&envelope)?,
    )?;
    let command = DurableCommandRecord::new(
        command.command_id().as_str(),
        1,
        Some(turn_id),
        command_binding(&command)?,
        component.clone(),
        DurableCommandDisposition::Accepted,
        None,
    )?;
    let operation = DurableOperationRecord::new(
        1,
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        runtime_component(RuntimeComponentKind::Operation, component.bytes().to_vec())?,
        vec![
            DurableOperationTransition::new(id(19_999)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(id(20_000)?, DurableOperationState::Dispatched),
        ],
    )?;
    let ledger = RuntimeOperationLedger::new(vec![command], vec![operation])?;
    let lifecycle = working_lifecycle(turn_id, &ledger)?;
    crate::checkpoint_bundle::tests::fixture_with_history_and_ledger(
        CanonicalHistory::default(),
        0,
        true,
        Some(ledger),
        Some(lifecycle),
    )
}

async fn seeded(
    source: &RuntimeCheckpoint,
) -> TestResult<(Arc<TestCreateStore>, crate::CreateReceipt)> {
    let store = Arc::new(TestCreateStore::default());
    let topology = source.manifest().topology();
    let created = store
        .create(CreateSession::new(
            SessionRef::new(topology.session_id, topology.branch_id),
            Timestamp::new(10, 0)?,
            id(40_000)?,
            NonZeroU64::new(3600).ok_or("lease")?,
            RuntimeCheckpointCodec::bundle(source, None)?,
        ))
        .await?;
    Ok((store, created))
}

fn fingerprint(store: &TestCreateStore) -> TestResult<String> {
    let accepted = lock_mutex(&store.accepted_commits).clone();
    let state = lock_mutex(&store.state);
    let mut bytes = Vec::new();
    for (checkpoint, facts) in accepted {
        bytes.extend_from_slice(checkpoint.id().as_uuid().as_bytes());
        bytes.extend_from_slice(checkpoint.checkpoint_commit_identity().bytes());
        bytes.extend_from_slice(checkpoint.integrity().bytes());
        for record in checkpoint.manifest().ledger().operations() {
            bytes.extend_from_slice(&crate::manifest::encode_bundle_operation_record(record)?);
        }
        let stored = state
            .stored_commits
            .get(&checkpoint.id())
            .ok_or("stored commit missing")?;
        bytes.extend_from_slice(stored.bundle().root().digest().bytes());
        for object in stored.bundle().supplied_objects() {
            bytes.push(object.address().kind() as u8);
            bytes.extend_from_slice(&object.address().codec_revision().to_be_bytes());
            bytes.extend_from_slice(object.address().digest().bytes());
            bytes.extend_from_slice(&(object.canonical_bytes().len() as u64).to_be_bytes());
            bytes.extend_from_slice(object.canonical_bytes());
            for child in object.children() {
                bytes.extend_from_slice(child.digest().bytes());
            }
        }
        bytes.extend_from_slice(&serde_json::to_vec(&facts)?);
    }
    Ok(Digest::sha256(
        DigestDomain::Blob,
        1,
        &[b"recovery-publication-golden-v1", &bytes],
    )?
    .to_string())
}

#[tokio::test]
async fn recovery_publish_original_code_golden() -> TestResult {
    let checkpoint = source(false).map_err(|error| format!("ordinary source: {error}"))?;
    let label = "effect";
    let target = CheckpointId::try_from(uuid::Uuid::parse_str(
        "bab6e05b-a7b1-8ba9-908c-361600b6fc09",
    )?)?;
    let (store, created) = seeded(&checkpoint)
        .await
        .map_err(|error| format!("{label} seed: {error}"))?;
    let mut lease = created.lease().clone();
    let mut owner = RecoveryPublicationOwner {
        store: store.clone(),
        store_timeout: Duration::from_secs(1),
        lease: &mut lease,
        head: created.branch(),
        checkpoint: Arc::new(checkpoint.clone()),
        bundle: RuntimeCheckpointCodec::bundle(&checkpoint, None)?,
    };
    let evidence = runtime_component(
        RuntimeComponentKind::Operation,
        serde_json::to_vec(&(label, [1, 2, 3], serde_json::Value::Null))?,
    )?;
    let record = DurableOperationRecord::new(
        2,
        DurableOperationKind::Effect {
            turn_id: id(7_001)?,
            attempt_id: id(7_002)?,
            call_id: id(7_003)?,
            source_index: 0,
        },
        runtime_component(RuntimeComponentKind::Operation, b"golden request".to_vec())?,
        vec![
            DurableOperationTransition::new(checkpoint.id(), DurableOperationState::Prepared),
            DurableOperationTransition::new(
                target,
                DurableOperationState::ProvenNotDispatched(evidence.clone()),
            ),
            DurableOperationTransition::new(target, DurableOperationState::Finalized(evidence)),
        ],
    )
    .map_err(|error| format!("{label} record: {error}"))?;
    let mut operations = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| record.operation_id() != 2)
        .cloned()
        .collect::<Vec<_>>();
    operations.push(record);
    let ledger = RuntimeOperationLedger::new(
        checkpoint
            .manifest()
            .ledger()
            .commands()
            .into_iter()
            .cloned()
            .collect(),
        operations,
    )
    .map_err(|error| format!("{label} ledger: {error}"))?;
    owner
        .publish(
            target,
            RuntimeLifecycle::Quiescent,
            checkpoint.manifest().canonical().history().clone(),
            ledger,
            checkpoint
                .manifest()
                .canonical()
                .context()
                .retained_for_exact_resume(),
            checkpoint.manifest().family_artifact().cloned(),
        )
        .await
        .map_err(|error| format!("{label} publish: {error}"))?;
    assert_eq!(store.commit_calls.load(Ordering::Acquire), 1);
    assert_eq!(
        fingerprint(&store)?,
        "faaf2d07f053838914a24bab9a388442ba3693dc987d2b7b778c5e675d2ec20d"
    );
    Ok(())
}

#[tokio::test]
async fn root_abandonment_original_code_golden() -> TestResult {
    let root = Arc::new(source(true).map_err(|error| format!("root source: {error}"))?);
    let (store, created) = seeded(&root).await?;
    let mut lease = created.lease().clone();
    let target = id(40_005)?;
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    crate::runtime::durable::turn_recovery::recover_root_turn_publication(
        &semantic_store,
        &mut lease,
        created.branch(),
        &root,
        &RuntimeCheckpointCodec::bundle(&root, None)?,
        crate::runtime::durable::turn_recovery::root_turn_closure_for_test(&root)?
            .ok_or("closure")?,
        root.manifest().lifecycle().obligations(),
        Duration::from_secs(1),
        target,
        None,
    )
    .await?;
    assert_eq!(store.commit_calls.load(Ordering::Acquire), 1);
    assert_eq!(
        fingerprint(&store)?,
        "9c1c24131b7f2ca764e096ffa6c6ffedbe5b9769827441a8842672b910ede80d"
    );
    Ok(())
}
