use super::*;

pub(super) fn durable_owner(agent: &Agent) -> Result<RunId, &'static str> {
    agent
        .inner
        .authority
        .lease_owner()
        .ok_or("agent omitted its durable lease owner")
}

#[tokio::test(start_paused = true)]
async fn retained_start_authority_rejects_elapsed_and_foreign_grants() -> TestResult {
    use crate::runtime::checkpoint::owner::{DurableBranchFacts, PreparedCheckpoint};

    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(20).unwrap(),
    )?;
    let agent = runtime
        .create(
            "create:lease-clock-admission",
            durable_artifact_test_builder(workspace, artifact_test_model(0, 45_000)?),
        )
        .await?;
    let checkpoint = agent.checkpoint();
    let original = store.active_lease().ok_or("seed lease")?;
    agent.shutdown().await?;
    *lock_mutex(&store.lease_read_override) = Some(lease_instant(80, 0)?);
    let retained = store
        .acquire(AcquireBranch::new(
            original.session(),
            original.owner(),
            NonZeroU64::new(20).unwrap(),
        ))
        .await?;
    let loaded = store.load_branch(original.session()).await?;
    let bundle = RuntimeCheckpointCodec::bundle(&checkpoint, None)?;
    let facts = DurableBranchFacts::new(
        store.clone(),
        loaded.head(),
        retained.clone(),
        crate::runtime::StoreTimeouts::default(),
    );
    let mut owner = PreparedCheckpoint::new(checkpoint, bundle, Some(facts)).start();
    let admission = owner.start_authority(CancellationToken::new());
    admission.check()?;

    // A second client acquires a newer fence after continuous time expires.
    *lock_mutex(&store.lease_read_override) = Some(lease_instant(110, 0)?);
    let second_client = Arc::clone(&store);
    let newer = second_client
        .acquire(AcquireBranch::new(
            retained.session(),
            RunId::try_from(uuid::Uuid::from_u128(981))?,
            retained.lease_seconds(),
        ))
        .await?;
    assert!(newer.fence() > retained.fence());
    assert_eq!(
        admission.check().unwrap_err().kind(),
        AgentErrorKind::OutcomeUnknown
    );

    // A foreign domain must fail even when its numeric reading looks live.
    *lock_mutex(&store.lease_read_override) = Some(LeaseInstant::new(
        LeaseDomain::new([3; 16], TEST_LEASE_DOMAIN.scope()),
        90,
        0,
    )?);
    assert_eq!(
        admission.check().unwrap_err().kind(),
        AgentErrorKind::OutcomeUnknown
    );
    *lock_mutex(&store.lease_read_override) = Some(retained.expires_at());
    assert_eq!(
        admission.check().unwrap_err().kind(),
        AgentErrorKind::OutcomeUnknown
    );
    *lock_mutex(&store.lease_read_override) = Some(lease_instant(90, 0)?);
    store.fail_lease_clock.store(true, Ordering::Release);
    assert_eq!(
        admission.check().unwrap_err().kind(),
        AgentErrorKind::OutcomeUnknown
    );
    let cleanup = owner.retire(Duration::from_secs(1), None).await;
    assert!(cleanup[0].is_ok());
    assert_eq!(
        cleanup[1].as_ref().unwrap_err().store_error(),
        Some(SessionStoreError::FenceSuperseded)
    );
    Ok(())
}

#[test]
fn command_identity_is_bounded_and_ack_is_stable() -> TestResult {
    let command_id = CommandId::new("queue-item:42")?;
    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(42))?;
    let accepted = CommandAck::accepted(command_id.clone(), 7, Some(turn_id));
    let duplicate = CommandAck::duplicate(command_id.clone(), &accepted);
    assert_eq!(duplicate.command_id(), &command_id);
    assert_eq!(duplicate.sequence(), 7);
    assert_eq!(duplicate.turn_id(), Some(turn_id));
    assert_eq!(duplicate.disposition(), CommandAckDisposition::Duplicate);
    assert!(CommandId::new("contains whitespace").is_err());
    Ok(())
}

#[test]
fn terminal_result_record_is_typed_canonical_and_tamper_closed() -> TestResult {
    let errors = [
        AgentError::from_model(ModelError::new(
            ModelErrorKind::Protocol,
            "sanitized protocol failure",
        )),
        AgentError::from_tool(&ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "sanitized tool failure",
        )),
        AgentError::from_store(SessionStoreError::BackendUnavailable),
    ];
    for error in errors {
        let durable = DurableAgentError::from_error(&error)?;
        let bytes = serde_json::to_vec(&durable)?;
        let decoded: DurableAgentError = serde_json::from_slice(&bytes)?;
        assert_eq!(decoded.into_error()?, error);
    }

    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(88_001))?;
    let pending = PendingTerminalResultRecord {
        remedy: None,
        version: TERMINAL_RESULT_RECORD_VERSION,
        command_id: "terminal:typed:1".to_owned(),
        turn_id,
        operation_id: OperationId(91),
        outcome: DurableTerminalOutcome::RecoveryRequired(DurableAgentError::from_error(
            &AgentError::new(AgentErrorKind::OutcomeUnknown, "exact recovery is required"),
        )?),
        usage: TurnUsage::default(),
    };
    let evidence = runtime_component(
        RuntimeComponentKind::Operation,
        encode_pending_terminal_record(pending)?,
    )?;
    let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(88_002))?;
    let conversation_identity = Digest::sha256(
        DigestDomain::CanonicalHistory,
        1,
        &[b"terminal-conversation"],
    )?;
    let finalized = finalized_operation_evidence(&evidence, checkpoint_id, conversation_identity)?;
    let record = decode_terminal_result_record(finalized.bytes())
        .ok_or("finalized terminal result did not decode")?;
    assert_eq!(record.command_id, "terminal:typed:1");
    assert_eq!(record.turn_id, turn_id);
    assert_eq!(record.operation_id, OperationId(91));
    assert_eq!(record.checkpoint_id, checkpoint_id);
    assert_eq!(record.conversation_identity, conversation_identity);
    assert!(matches!(
        record.outcome,
        DurableTerminalOutcome::RecoveryRequired(_)
    ));
    let mut tampered = finalized.bytes().to_vec();
    let last = tampered.last_mut().ok_or("terminal record was empty")?;
    *last ^= 1;
    assert!(decode_terminal_result_record(&tampered).is_none());
    Ok(())
}

/// A session written by an earlier candidate still reopens.
///
/// 2026-09-18, attempt 5 with a clean control: a session created by the
/// candidate 2 binary could not be reopened by candidate 5. Exit 5,
/// "accepted root command omitted its canonical terminal result record", and
/// candidate 2 reopened the same session immediately afterwards.
///
/// The record was there. `decode_terminal_result_record` re-encodes what it
/// decoded and demands the bytes back byte for byte, and `TurnUsage` had gained
/// `peak_input_tokens`, which decoded from nothing but re-encoded as `null`.
/// The round trip therefore disagreed with the stored bytes, the decoder
/// answered `None`, and the caller read `None` as "omitted". No decode error
/// was ever reported, which is why the optional field looked innocent.
///
/// The rule this pins: an absent field is unknown, never a refusal, and it
/// survives the round trip by staying absent.
#[test]
fn a_terminal_record_without_a_peak_round_trips_byte_for_byte() -> TestResult {
    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(88_101))?;
    let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(88_102))?;
    let conversation_identity = Digest::sha256(
        DigestDomain::CanonicalHistory,
        1,
        &[b"earlier-candidate-conversation"],
    )?;
    let finalize = |usage: TurnUsage| -> TestResult<Vec<u8>> {
        let pending = PendingTerminalResultRecord {
            remedy: None,
            version: TERMINAL_RESULT_RECORD_VERSION,
            command_id: "terminal:earlier-candidate:1".to_owned(),
            turn_id,
            operation_id: OperationId(7),
            outcome: DurableTerminalOutcome::RecoveryRequired(DurableAgentError::from_error(
                &AgentError::new(AgentErrorKind::OutcomeUnknown, "exact recovery is required"),
            )?),
            usage,
        };
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            encode_pending_terminal_record(pending)?,
        )?;
        Ok(
            finalized_operation_evidence(&evidence, checkpoint_id, conversation_identity)?
                .bytes()
                .to_vec(),
        )
    };

    // Exactly the shape an earlier candidate wrote: no peak anywhere.
    let earlier = finalize(TurnUsage::default())?;
    assert!(
        !String::from_utf8_lossy(&earlier).contains("peak_input_tokens"),
        "an unknown peak must not be written at all, or older stores stop matching"
    );
    let decoded = decode_terminal_result_record(&earlier)
        .ok_or("a record written before the peak existed must still decode")?;
    assert_eq!(decoded.usage.peak_input_tokens(), None);
    assert_eq!(decoded.turn_id, turn_id);

    // A record that does know its peak keeps it, and round trips as itself.
    let mut usage = TurnUsage::default();
    usage.record_model_operation(Some(Usage::new(31_500, 40, 0, 0, 0)))?;
    let current = finalize(usage)?;
    assert!(String::from_utf8_lossy(&current).contains("peak_input_tokens"));
    let decoded =
        decode_terminal_result_record(&current).ok_or("a record carrying a peak must decode")?;
    assert_eq!(decoded.usage.peak_input_tokens(), Some(31_500));
    Ok(())
}

/// A store written by an earlier candidate reopens through the real path.
///
/// This pins the attempt 5 blocker: a candidate 2 session
/// could not be reopened by candidate 5 because `TurnUsage` had gained an
/// optional field that re-encoded as `null`. Here the record is built in the
/// exact shape an earlier candidate wrote — no `peak_input_tokens` anywhere —
/// and handed to `terminal_record_for_command`, the function whose refusal the
/// operator actually saw.
#[test]
fn an_earlier_candidates_terminal_record_reopens_and_a_broken_one_says_why() -> TestResult {
    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(88_201))?;
    let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(88_202))?;
    let binding = Digest::sha256(DigestDomain::ProviderOperation, 4, &[b"earlier-root"])?;
    let command_bytes =
        serde_json::to_vec(&("root:earlier", 1_u64, turn_id, binding, "prompt", false))?;
    let command = DurableCommandRecord::new(
        "root:earlier",
        1,
        Some(turn_id),
        binding,
        runtime_component(RuntimeComponentKind::Command, command_bytes.clone())?,
        DurableCommandDisposition::Accepted,
        None,
    )?;
    let terminal_bytes = |usage: TurnUsage| -> TestResult<Vec<u8>> {
        let pending = PendingTerminalResultRecord {
            remedy: None,
            version: TERMINAL_RESULT_RECORD_VERSION,
            command_id: "root:earlier".to_owned(),
            turn_id,
            operation_id: OperationId(1),
            outcome: DurableTerminalOutcome::RecoveryRequired(DurableAgentError::from_error(
                &AgentError::new(AgentErrorKind::OutcomeUnknown, "exact recovery is required"),
            )?),
            usage,
        };
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            encode_pending_terminal_record(pending)?,
        )?;
        let conversation_identity = Digest::sha256(
            DigestDomain::CanonicalHistory,
            1,
            &[b"earlier-conversation"],
        )?;
        Ok(
            finalized_operation_evidence(&evidence, checkpoint_id, conversation_identity)?
                .bytes()
                .to_vec(),
        )
    };
    let reopen = |terminal: Vec<u8>| -> TestResult<Result<(), String>> {
        let operation = DurableOperationRecord::new(
            1,
            DurableOperationKind::Command {
                turn_id: Some(turn_id),
            },
            runtime_component(RuntimeComponentKind::Operation, command_bytes.clone())?,
            vec![DurableOperationTransition::new(
                checkpoint_id,
                DurableOperationState::Finalized(runtime_component(
                    RuntimeComponentKind::Operation,
                    terminal,
                )?),
            )],
        )?;
        let ledger = RuntimeOperationLedger::new(vec![command.clone()], vec![operation])?;
        Ok(terminal_record_for_command(&ledger, &command)
            .map(|_| ())
            .map_err(|error| error.message().to_owned()))
    };

    // An earlier candidate's shape: the optional field is absent everywhere.
    let earlier = terminal_bytes(TurnUsage::default())?;
    assert!(
        !String::from_utf8_lossy(&earlier).contains("peak_input_tokens"),
        "this fixture must be the earlier shape or it pins nothing"
    );
    assert_eq!(
        reopen(earlier.clone())?,
        Ok(()),
        "a session written by an earlier candidate must reopen"
    );

    // Present but re-encoding differently (the peak out of field order): the
    // refusal names the round trip.
    let explicit_null = String::from_utf8(earlier)?
        .replacen(r#""usage":{"#, r#""usage":{"peak_input_tokens":31500,"#, 1)
        .into_bytes();
    assert!(matches!(
        read_terminal_result_record(&explicit_null),
        Err(TerminalRecordRejection::RoundTrip(_))
    ));
    let message = reopen(explicit_null)?.expect_err("a changed re-encoding must refuse");
    assert!(
        message.contains("re-encodes differently"),
        "the refusal must not call a present record missing: {message}"
    );
    assert!(
        message.contains("peak_input_tokens"),
        "the refusal must name where the bytes diverge: {message}"
    );
    assert!(
        message.contains("first differs at byte"),
        "the refusal must locate the divergence: {message}"
    );

    // Genuinely absent: the older wording, for the case it was always meant for.
    let message = reopen(b"not a terminal record at all".to_vec())?
        .expect_err("bytes that are not a record must refuse");
    assert!(
        message.contains("no canonical terminal result record"),
        "{message}"
    );
    assert!(
        !message.contains("re-encodes differently"),
        "an absent record must not be reported as a round-trip failure: {message}"
    );
    Ok(())
}

#[test]
fn operation_ledger_accepts_only_exact_checkpoint_bound_transitions() -> TestResult {
    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(40))?;
    let attempt_id = AttemptId::try_from(uuid::Uuid::from_u128(41))?;
    let prepared = CheckpointId::try_from(uuid::Uuid::from_u128(42))?;
    let dispatched = CheckpointId::try_from(uuid::Uuid::from_u128(43))?;
    let terminal = CheckpointId::try_from(uuid::Uuid::from_u128(44))?;
    let binding = Digest::sha256(DigestDomain::ProviderOperation, 2, &[b"request"])?;
    let outcome = operation_binding_component(Digest::sha256(
        DigestDomain::ProviderOperation,
        2,
        &[b"response"],
    )?)?;
    let record = DurableOperationRecord::new(
        1,
        DurableOperationKind::Provider {
            root: ModelOperationRoot::Turn(turn_id),
            attempt_id,
        },
        operation_binding_component(binding)?,
        vec![DurableOperationTransition::new(
            prepared,
            DurableOperationState::Prepared,
        )],
    )?;

    assert!(
        record
            .append_transition(DurableOperationTransition::new(
                terminal,
                DurableOperationState::Finalized(outcome.clone()),
            ))
            .is_err()
    );
    let record = record.append_transition(DurableOperationTransition::new(
        dispatched,
        DurableOperationState::Dispatched,
    ))?;
    let record = record.append_transition(DurableOperationTransition::new(
        terminal,
        DurableOperationState::Completed(outcome.clone()),
    ))?;
    assert_eq!(
        record.state(),
        &DurableOperationState::Completed(outcome.clone())
    );
    assert!(
        record
            .append_transition(DurableOperationTransition::new(
                terminal,
                DurableOperationState::OutcomeUnknown(outcome),
            ))
            .is_err()
    );
    Ok(())
}

#[test]
fn prepared_operation_may_close_only_with_non_dispatch_proof() -> TestResult {
    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(50))?;
    let prepared = CheckpointId::try_from(uuid::Uuid::from_u128(51))?;
    let terminal = CheckpointId::try_from(uuid::Uuid::from_u128(52))?;
    let binding = Digest::sha256(DigestDomain::ProviderOperation, 2, &[b"binding"])?;
    let evidence = operation_binding_component(Digest::sha256(
        DigestDomain::ProviderOperation,
        2,
        &[b"not-dispatched"],
    )?)?;
    let record = DurableOperationRecord::new(
        1,
        DurableOperationKind::Commit {
            root: ModelOperationRoot::Turn(turn_id),
        },
        operation_binding_component(binding)?,
        vec![DurableOperationTransition::new(
            prepared,
            DurableOperationState::Prepared,
        )],
    )?
    .append_transition(DurableOperationTransition::new(
        terminal,
        DurableOperationState::ProvenNotDispatched(evidence.clone()),
    ))?;
    assert_eq!(
        record.state(),
        &DurableOperationState::ProvenNotDispatched(evidence)
    );
    Ok(())
}

#[test]
fn provider_finalization_requires_exact_dispatch_rejection_proof() -> TestResult {
    let prepared_call = Digest::sha256(
        DigestDomain::ProviderOperation,
        plakous_model::PROVIDER_OPERATION_SEMANTIC_REVISION,
        &[b"prepared-call"],
    )?;
    let attempt_id = AttemptId::try_from(uuid::Uuid::from_u128(53))?;
    let proof = plakous_model::ModelDispatchRejection::for_prepared_call(
        plakous_model::ModelDispatchRejectionClass::Admission,
        plakous_model::ModelDispatchRejectionPhase::Revalidation,
        prepared_call,
        attempt_id,
    );
    let terminal_state = |error: &AgentError, expected| -> Result<_, AgentError> {
        let outcome = crate::runtime::operation_record::ProviderOutcome {
            result: crate::runtime::operation_record::ProviderAttemptResult::Failure(
                DurableAgentError::from_error(error)?,
            ),
            usage: None,
            decision: crate::runtime::operation_record::RoundDecision::FailTurn(
                crate::runtime::operation_record::FailTurnShape {
                    failure: DurableAgentError::from_error(error)?,
                    prompt_withheld: false,
                    remedy: None,
                },
            ),
            budgets: crate::runtime::operation_record::TurnBudgets::default(),
            steering: Vec::new(),
        };
        provider_terminal_state(Err(error), &outcome, true, expected)
    };
    let exact = AgentError::from_model(
        ModelError::new(ModelErrorKind::ResourceLimit, "admission rejected")
            .with_dispatch_rejection(proof)?,
    );
    assert!(matches!(
        terminal_state(&exact, Some((prepared_call, attempt_id)))?,
        DurableOperationState::ProvenNotDispatched(_)
    ));

    let foreign_attempt = AttemptId::try_from(uuid::Uuid::from_u128(54))?;
    assert!(matches!(
        terminal_state(&exact, Some((prepared_call, foreign_attempt)))?,
        DurableOperationState::OutcomeUnknown(_)
    ));
    let absent = AgentError::from_model(ModelError::new(
        ModelErrorKind::ResourceLimit,
        "unproven admission rejection",
    ));
    assert!(matches!(
        terminal_state(&absent, Some((prepared_call, attempt_id)))?,
        DurableOperationState::Completed(_)
    ));
    Ok(())
}

#[test]
fn working_lifecycle_coalesces_dispatched_turn_with_compaction_session_workflow() -> TestResult {
    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(96))?;
    let attempt_id = AttemptId::try_from(uuid::Uuid::from_u128(97))?;
    let workflow_id = SessionWorkflowRootId::new([0xa6; 32])?;
    let prepared = CheckpointId::try_from(uuid::Uuid::from_u128(98))?;
    let dispatched = CheckpointId::try_from(uuid::Uuid::from_u128(99))?;
    let component = |label: &'static [u8]| -> Result<_, Box<dyn std::error::Error>> {
        Ok(operation_binding_component(Digest::sha256(
            DigestDomain::ProviderOperation,
            2,
            &[label],
        )?)?)
    };
    let command = DurableOperationRecord::new(
        1,
        DurableOperationKind::Command {
            turn_id: Some(turn_id),
        },
        component(b"command")?,
        vec![
            DurableOperationTransition::new(prepared, DurableOperationState::Prepared),
            DurableOperationTransition::new(dispatched, DurableOperationState::Dispatched),
        ],
    )?;
    let compaction = DurableOperationRecord::new(
        2,
        DurableOperationKind::Compaction { workflow_id },
        component(b"compaction")?,
        vec![DurableOperationTransition::new(
            dispatched,
            DurableOperationState::Prepared,
        )],
    )?;
    let provider = DurableOperationRecord::new(
        3,
        DurableOperationKind::Provider {
            root: ModelOperationRoot::SessionWorkflow(workflow_id),
            attempt_id,
        },
        component(b"provider")?,
        vec![DurableOperationTransition::new(
            dispatched,
            DurableOperationState::Prepared,
        )],
    )?;
    let ledger = RuntimeOperationLedger::new(Vec::new(), vec![command, compaction, provider])?;
    let lifecycle =
        working_lifecycle_for_root(ModelOperationRoot::SessionWorkflow(workflow_id), &ledger)?;
    match lifecycle {
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn {
                turn_id: observed, ..
            },
            recovery_phase,
            obligations,
        } => {
            assert_eq!(observed, turn_id);
            assert_eq!(recovery_phase, WorkingRecoveryPhase::DispatchEvidence);
            assert_eq!(obligations.len(), 3);
        }
        other => return Err(format!("expected Working Turn, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn working_lifecycle_rejects_two_distinct_session_workflows() -> TestResult {
    let first = SessionWorkflowRootId::new([0xa7; 32])?;
    let second = SessionWorkflowRootId::new([0xa8; 32])?;
    let prepared = CheckpointId::try_from(uuid::Uuid::from_u128(100))?;
    let component = operation_binding_component(Digest::sha256(
        DigestDomain::ProviderOperation,
        2,
        &[b"compaction"],
    )?)?;
    let ledger = RuntimeOperationLedger::new(
        Vec::new(),
        vec![
            DurableOperationRecord::new(
                1,
                DurableOperationKind::Compaction { workflow_id: first },
                component.clone(),
                vec![DurableOperationTransition::new(
                    prepared,
                    DurableOperationState::Prepared,
                )],
            )?,
            DurableOperationRecord::new(
                2,
                DurableOperationKind::Compaction {
                    workflow_id: second,
                },
                component,
                vec![DurableOperationTransition::new(
                    prepared,
                    DurableOperationState::Prepared,
                )],
            )?,
        ],
    )?;
    let error = working_lifecycle_for_root(ModelOperationRoot::SessionWorkflow(first), &ledger)
        .expect_err("two session workflows must not coalesce");
    assert_eq!(error.kind(), AgentErrorKind::Internal);
    assert!(
        error
            .to_string()
            .contains("durable working set mixes logical operation roots")
    );
    Ok(())
}

#[test]
fn compaction_receipt_allows_working_to_working_failure() -> TestResult {
    let working = RuntimeLifecycle::Working {
        root: RuntimeWorkingRoot::Turn {
            turn_id: TurnId::try_from(uuid::Uuid::from_u128(0x301))?,
            next_round: 1,
        },
        recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
        obligations: Vec::new(),
    };
    assert!(compaction_receipt_lifecycle_ok(&working, &working, true));
    assert!(compaction_receipt_lifecycle_ok(&working, &working, false));
    assert!(compaction_receipt_lifecycle_ok(
        &working,
        &RuntimeLifecycle::Quiescent,
        false
    ));
    assert!(!compaction_receipt_lifecycle_ok(
        &working,
        &RuntimeLifecycle::Quiescent,
        true
    ));
    assert!(compaction_receipt_lifecycle_ok(
        &RuntimeLifecycle::Quiescent,
        &RuntimeLifecycle::Quiescent,
        false
    ));
    Ok(())
}

#[test]
fn checkpoint_ids_are_run_scoped_deterministic_uuid_v8_values() -> TestResult {
    let run_id = RunId::try_from(uuid::Uuid::from_u128(60))?;
    let first = deterministic_checkpoint_id(run_id, 1)?;
    let repeated = deterministic_checkpoint_id(run_id, 1)?;
    let second = deterministic_checkpoint_id(run_id, 2)?;

    assert_eq!(first, repeated);
    assert_ne!(first, second);
    assert_eq!(first.as_uuid().as_bytes()[6] >> 4, 8);
    assert_eq!(first.as_uuid().as_bytes()[8] >> 6, 2);
    Ok(())
}

#[test]
fn provider_and_parallel_effect_crash_cuts_have_exact_recovery_phases() -> TestResult {
    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(70))?;
    let attempt_id = AttemptId::try_from(uuid::Uuid::from_u128(71))?;
    let call_a = CallId::try_from(uuid::Uuid::from_u128(72))?;
    let call_b = CallId::try_from(uuid::Uuid::from_u128(73))?;
    let checkpoint = |value| CheckpointId::try_from(uuid::Uuid::from_u128(value));
    let component = |label: &'static [u8]| -> Result<_, Box<dyn std::error::Error>> {
        Ok(operation_binding_component(Digest::sha256(
            DigestDomain::ProviderOperation,
            2,
            &[label],
        )?)?)
    };

    let provider = DurableOperationRecord::new(
        1,
        DurableOperationKind::Provider {
            root: ModelOperationRoot::Turn(turn_id),
            attempt_id,
        },
        component(b"provider")?,
        vec![DurableOperationTransition::new(
            checkpoint(74)?,
            DurableOperationState::Prepared,
        )],
    )?;
    let ledger = RuntimeOperationLedger::new(Vec::new(), vec![provider])?;
    assert!(matches!(
        working_lifecycle(turn_id, &ledger)?,
        RuntimeLifecycle::Working {
            recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
            ..
        }
    ));
    let ledger = ledger.transition_operation(
        1,
        DurableOperationTransition::new(checkpoint(75)?, DurableOperationState::Dispatched),
    )?;
    assert!(matches!(
        working_lifecycle(turn_id, &ledger)?,
        RuntimeLifecycle::Working {
            recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
            ..
        }
    ));

    let effect_a = DurableOperationRecord::new(
        2,
        DurableOperationKind::Effect {
            turn_id,
            attempt_id,
            call_id: call_a,
            source_index: 0,
        },
        component(b"effect-a")?,
        vec![
            DurableOperationTransition::new(checkpoint(76)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(
                checkpoint(77)?,
                DurableOperationState::Claimed(component(b"claimed-a")?),
            ),
            DurableOperationTransition::new(checkpoint(78)?, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                checkpoint(79)?,
                DurableOperationState::Completed(component(b"completed")?),
            ),
        ],
    )?;
    let effect_b = DurableOperationRecord::new(
        3,
        DurableOperationKind::Effect {
            turn_id,
            attempt_id,
            call_id: call_b,
            source_index: 1,
        },
        component(b"effect-b")?,
        vec![
            DurableOperationTransition::new(checkpoint(80)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(
                checkpoint(81)?,
                DurableOperationState::Claimed(component(b"claimed-b")?),
            ),
            DurableOperationTransition::new(checkpoint(82)?, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                checkpoint(83)?,
                DurableOperationState::OutcomeUnknown(component(b"unknown")?),
            ),
        ],
    )?;
    let mixed = RuntimeOperationLedger::new(Vec::new(), vec![effect_a, effect_b])?;
    let RuntimeLifecycle::Working {
        recovery_phase,
        obligations,
        ..
    } = working_lifecycle(turn_id, &mixed)?
    else {
        return Err("mixed effect evidence unexpectedly quiesced".into());
    };
    assert_eq!(recovery_phase, WorkingRecoveryPhase::EffectReconciliation);
    assert_eq!(obligations.len(), 2);
    assert!(
        obligations
            .iter()
            .any(|item| item.action() == RecoveryAction::FinalizationRequired)
    );
    assert!(
        obligations
            .iter()
            .any(|item| item.action() == RecoveryAction::EffectEvidenceRequired)
    );
    Ok(())
}

#[test]
#[allow(
    clippy::items_after_statements,
    reason = "the local Debug-only witness stays adjacent to the assertion that durable identity ignores Debug output"
)]
fn checkpoint_commit_identity_golden_matrix_is_debug_independent() -> TestResult {
    let obligation = |operation_id, kind, action| {
        Ok::<_, crate::RuntimeManifestError>(RecoveryObligation::new(
            operation_id,
            RecoveryHandlerSignature::new(kind, action),
        ))
    };
    let cases = [
        (
            RuntimeLifecycle::Quiescent,
            "b0b5b49c4d8e2dad9475a3e32924061200bc3b31f52e61d0921a674828fe90ae",
        ),
        (
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::Turn {
                    turn_id: TurnId::try_from(uuid::Uuid::from_u128(0x100))?,
                    next_round: 1,
                },
                recovery_phase: WorkingRecoveryPhase::DispatchEvidence,
                obligations: vec![
                    obligation(
                        11,
                        RecoveryObligationKind::Command,
                        RecoveryAction::ProvenNotDispatched,
                    )?,
                    obligation(
                        12,
                        RecoveryObligationKind::ProviderAttempt,
                        RecoveryAction::ProviderEvidenceRequired,
                    )?,
                ],
            },
            "19c64f931fa3ef13e497bb7eb62f96465cba9cbfc5a97899ce3f9c83c307e4e4",
        ),
        (
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::Turn {
                    turn_id: TurnId::try_from(uuid::Uuid::from_u128(0x200))?,
                    next_round: 2,
                },
                recovery_phase: WorkingRecoveryPhase::EffectReconciliation,
                obligations: vec![
                    obligation(
                        13,
                        RecoveryObligationKind::EffectDecision,
                        RecoveryAction::FinalizationRequired,
                    )?,
                    obligation(
                        14,
                        RecoveryObligationKind::Effect,
                        RecoveryAction::EffectEvidenceRequired,
                    )?,
                ],
            },
            "47294861d308d10ebe504222ce9f988157e677d4125fd1fc14e3f9ccb2f850d9",
        ),
        (
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::Turn {
                    turn_id: TurnId::try_from(uuid::Uuid::from_u128(0x300))?,
                    next_round: 3,
                },
                recovery_phase: WorkingRecoveryPhase::CommitRecovery,
                obligations: vec![obligation(
                    15,
                    RecoveryObligationKind::Commit,
                    RecoveryAction::CommitRecovery,
                )?],
            },
            "c627e96623b669c4f27c9b63df1f0cc490e068c8d33d3b5c0548f935f57b65a8",
        ),
        (
            RuntimeLifecycle::Working {
                root: RuntimeWorkingRoot::Turn {
                    turn_id: TurnId::try_from(uuid::Uuid::from_u128(0x400))?,
                    next_round: 4,
                },
                recovery_phase: WorkingRecoveryPhase::Finalization,
                obligations: vec![obligation(
                    16,
                    RecoveryObligationKind::ProviderAttempt,
                    RecoveryAction::FinalizationRequired,
                )?],
            },
            "f9e48e0a2141f656f4c5e34ec7236db9d5002e6993a45b2ad628f23ce4c7a84c",
        ),
    ];
    let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(0x600))?;
    let parent_checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(0x601))?;
    let history_identity = Digest::sha256(DigestDomain::CanonicalHistory, 1, &[b"history"])?;
    let context_identity = Digest::sha256(DigestDomain::ContextBudget, 1, &[b"context"])?;
    let ledger = RuntimeOperationLedger::new(Vec::new(), Vec::new())?;
    let actual = cases
        .iter()
        .map(|(lifecycle, _)| {
            checkpoint_commit_identity(CheckpointCommitFacts {
                checkpoint_id,
                parent_checkpoint_id,
                branch_revision: 7,
                lifecycle,
                history_identity,
                ledger: &ledger,
                context_identity,
                family_artifact: None,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert!(
        actual
            .iter()
            .all(|identity| identity.version() == CHECKPOINT_COMMIT_IDENTITY_REVISION)
    );
    assert_eq!(
        actual.iter().map(ToString::to_string).collect::<Vec<_>>(),
        cases
            .iter()
            .map(|(_, expected)| (*expected).to_owned())
            .collect::<Vec<_>>()
    );

    struct AlternateDebug<'a>(&'a RuntimeLifecycle, &'static str);
    impl fmt::Debug for AlternateDebug<'_> {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.1)
        }
    }
    let terse = AlternateDebug(&cases[1].0, "working");
    let refactored = AlternateDebug(&cases[1].0, "renamed fields and variants");
    assert_ne!(format!("{terse:?}"), format!("{refactored:?}"));
    assert_eq!(
        checkpoint_commit_identity(CheckpointCommitFacts {
            checkpoint_id,
            parent_checkpoint_id,
            branch_revision: 7,
            lifecycle: terse.0,
            history_identity,
            ledger: &ledger,
            context_identity,
            family_artifact: None,
        })?,
        checkpoint_commit_identity(CheckpointCommitFacts {
            checkpoint_id,
            parent_checkpoint_id,
            branch_revision: 7,
            lifecycle: refactored.0,
            history_identity,
            ledger: &ledger,
            context_identity,
            family_artifact: None,
        })?
    );
    assert_ne!(
        checkpoint_commit_identity(CheckpointCommitFacts {
            checkpoint_id,
            parent_checkpoint_id,
            branch_revision: 7,
            lifecycle: &cases[0].0,
            history_identity,
            ledger: &ledger,
            context_identity,
            family_artifact: None,
        })?,
        checkpoint_commit_identity(CheckpointCommitFacts {
            checkpoint_id,
            parent_checkpoint_id,
            branch_revision: 7,
            lifecycle: &cases[0].0,
            history_identity,
            ledger: &ledger,
            context_identity: Digest::sha256(DigestDomain::ContextBudget, 1, &[b"other-context"])?,
            family_artifact: None,
        })?
    );
    Ok(())
}

#[test]
fn lost_ack_recovery_requires_exact_old_new_and_commit_identity() -> TestResult {
    let session = SessionRef::new(
        SessionId::try_from(uuid::Uuid::from_u128(90))?,
        BranchId::try_from(uuid::Uuid::from_u128(91))?,
    );
    let owner = RunId::try_from(uuid::Uuid::from_u128(92))?;
    let lease = BranchLease::from_store(
        session,
        owner,
        NonZeroU64::new(4).unwrap(),
        lease_instant(100, 0)?,
        NonZeroU64::new(30).unwrap(),
    );
    let address =
        |label: &'static [u8]| -> Result<RuntimeObjectAddress, Box<dyn std::error::Error>> {
            Ok(RuntimeObjectAddress::from_store_parts(
                RuntimeObjectKind::ManifestRoot,
                RUNTIME_FORMAT_EPOCH,
                Digest::sha256(DigestDomain::Blob, RUNTIME_FORMAT_EPOCH, &[label])?,
            )?)
        };
    let integrity =
        |label: &'static [u8]| Digest::sha256(DigestDomain::ProviderOperation, 3, &[label]);
    let old_ref = CheckpointObjectRef::from_store(
        CheckpointId::try_from(uuid::Uuid::from_u128(93))?,
        address(b"old")?,
        integrity(b"old")?,
    );
    let new_ref = CheckpointObjectRef::from_store(
        CheckpointId::try_from(uuid::Uuid::from_u128(94))?,
        address(b"new")?,
        integrity(b"new")?,
    );
    let old = BranchHead::from_store(session, BranchRevision::INITIAL, old_ref);
    let new = BranchHead::from_store(session, BranchRevision::from_store(1), new_ref);
    let commit_identity = Digest::sha256(DigestDomain::ProviderOperation, 3, &[b"commit"])?;
    let recovered = CommittedCheckpoint::from_store(
        CommitOperation::from_store(lease.clone(), old, new_ref, commit_identity),
        CommitReceipt::from_store(new_ref, old, new, crate::feed::FeedSeq::new(2)),
    );
    assert!(recovered_commit_matches_exact(
        &recovered,
        &lease,
        old,
        new_ref,
        commit_identity,
    ));
    assert!(!recovered_commit_matches_exact(
        &recovered,
        &lease,
        new,
        new_ref,
        commit_identity,
    ));
    assert!(!recovered_commit_matches_exact(
        &recovered,
        &lease,
        old,
        old_ref,
        commit_identity,
    ));
    assert!(!recovered_commit_matches_exact(
        &recovered,
        &lease,
        old,
        new_ref,
        Digest::sha256(DigestDomain::ProviderOperation, 3, &[b"other-commit"],)?,
    ));
    Ok(())
}
