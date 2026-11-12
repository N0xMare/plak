//! Injected multi-minute incomplete-write outage followed by an authoritative head.

use super::*;

#[tokio::test]
async fn revision_twelve_to_thirteen_changes_attempt_execution_not_history_authority() -> TestResult
{
    let client = Client::new(Limits::default())?;
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let current = attempt_data(&client, &authorization)?;
    let transport = client.behavior_identity().ok_or("transport identity")?;
    let mut previous = current.clone();
    previous.host_behavior = Digest::sha256(
        DigestDomain::ModelBinding,
        12,
        &[
            b"responses-attempt-host-behavior-v12",
            transport.as_bytes().as_slice(),
            1_u32.to_be_bytes().as_slice(),
            1_u32.to_be_bytes().as_slice(),
        ],
    )?;
    previous.execution = dialect_execution_identity(
        1,
        previous.target,
        previous.host_behavior,
        previous.ledger_policy,
        previous.auth_slots,
        previous.request_policy,
        &previous.stream,
        previous.stream_limits,
        &previous.replay,
        previous.storage,
        previous.provider_error_policy,
        previous.error_limits,
        previous.provider_request_id_header.as_ref(),
        previous.allow_missing_sse_content_type,
    )?;
    let (request, authority) = exact_request_authority()?;
    let old = prepare_responses_attempt(authority.clone(), &request, None, &previous)?;
    let fresh = prepare_responses_attempt(authority, &request, None, &current)?;
    assert_ne!(old.operation().execution(), fresh.operation().execution());
    assert_eq!(old.authority().identity(), fresh.authority().identity());
    assert_eq!(
        old.authority().artifact_binding(),
        fresh.authority().artifact_binding()
    );
    assert_eq!(old.body(), fresh.body());
    let dialect = wire_dialect(
        Origin::parse("https://127.0.0.1:9")?,
        openai_generation_fingerprint(b"upgrade"),
    );
    let service =
        ResponsesAttemptService::new(client, Arc::clone(&dialect), current, authorization)?;
    let old = ResponsesAttempt::from_prepared(
        old,
        typed_id::<AttemptId>(14)?,
        CancellationToken::new(),
        monotonic_now() + Duration::from_secs(5),
    );
    let Err(error) = service.execute(old).await else {
        panic!("an old prepared attempt must be refused");
    };
    assert!(matches!(error, ResponsesAttemptError::Binding { .. }));
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cancelling_an_incomplete_write_wait_prevents_later_dispatch() -> TestResult {
    let client = Client::new(Limits::default())?
        .with_faults(Some(plakous_http::FaultPlan::parse("fault=write-reset")?));
    let dialect = wire_dialect(
        Origin::parse("https://127.0.0.1:9")?,
        openai_generation_fingerprint(b"cancel-incomplete"),
    );
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let cancellation = CancellationToken::new();
    let (sender, mut observations) = mpsc::channel(64);
    let attempt = executable_attempt(&data, cancellation.clone())?.with_observations(sender);
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
    let running = tokio::spawn(async move { service.execute(attempt).await });
    while let Some(observation) = observations.recv().await {
        if matches!(observation, ResponsesObservation::Retry(_)) {
            cancellation.cancel();
            break;
        }
    }
    let Err(error) = running.await? else {
        panic!("cancelled incomplete-write wait must not succeed");
    };
    assert!(matches!(error, ResponsesAttemptError::Cancelled { .. }));
    tokio::time::advance(Duration::from_secs(900)).await;
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 1);
    assert_eq!(dialect.guards_dropped.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn incomplete_write_outage_preserves_ordinary_retries_and_workflow_single_dispatch()
-> TestResult {
    for workflow in [false, true] {
        for fault in ["write-reset", "record-rejected"] {
            let mut rules: Vec<_> = (1..=8).map(|n| format!("nth={n},fault={fault}")).collect();
            rules.push("fault=503".into());
            let client = Client::new(Limits::default())?
                .with_faults(Some(plakous_http::FaultPlan::parse(&rules.join(";"))?));
            let dialect = wire_dialect(
                Origin::parse("https://127.0.0.1:9")?,
                openai_generation_fingerprint(b"incomplete-outage"),
            );
            let authorization =
                ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
            let data = attempt_data(&client, &authorization)?;
            let attempt = if workflow {
                let metadata = RequestMetadata::for_session_workflow(
                    typed_id::<RunId>(10)?,
                    SessionWorkflowRootId::new([7; 32])?,
                    typed_id::<AttemptId>(12)?,
                    Timestamp::new(1_700_000_000, 0)?,
                );
                let (request, authority) = exact_request_authority_with_metadata(false, metadata)?;
                ResponsesAttempt::from_prepared(
                    prepare_responses_attempt(authority, &request, None, &data)?,
                    typed_id::<AttemptId>(14)?,
                    CancellationToken::new(),
                    monotonic_now() + Duration::from_secs(5),
                )
            } else {
                ResponsesAttempt::from_prepared(
                    prepared_attempt(&data)?,
                    typed_id::<AttemptId>(14)?,
                    CancellationToken::new(),
                    monotonic_now() + Duration::from_secs(5),
                )
            };
            let service =
                ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
            let started = monotonic_now();
            let Err(error) = service.execute(attempt).await else {
                panic!("injected authoritative refusal must not succeed");
            };
            let facts = error.facts();
            assert!(
                matches!(
                    error,
                    ResponsesAttemptError::HttpStatus {
                        status: StatusCode::SERVICE_UNAVAILABLE,
                        ..
                    }
                ),
                "{fault}, workflow={workflow}: {error:?}"
            );
            let expected = if workflow { 9 } else { 12 };
            assert_eq!(facts.physical_attempts(), expected);
            assert_eq!(facts.summary().ride_out_physical, 8);
            assert_eq!(facts.failure_risk(), FailureRisk::None);
            assert!(monotonic_now().duration_since(started) >= Duration::from_secs(180));
            assert_eq!(dialect.preflights.load(Ordering::SeqCst), expected as usize);
            assert_eq!(
                dialect.final_authorizations.load(Ordering::SeqCst),
                expected as usize
            );
            assert_eq!(
                dialect.guards_dropped.load(Ordering::SeqCst),
                expected as usize
            );
        }
    }
    Ok(())
}
