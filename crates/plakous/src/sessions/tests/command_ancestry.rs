use super::*;

#[tokio::test]
async fn command_admission_sequences_follow_fork_ancestry_not_sibling_branches() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store),
        RuntimeDurability::Disabled,
    ))?;
    let binding =
        || completing_runtime_binding_for(workspace.clone(), descriptor()?, b"ancestry-account");
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:ancestry")?,
            binding()?,
        ))
        .await?;
    let inherited = CommandId::new("root:inherited")?;
    let started = session
        .agent()
        .start(inherited.clone(), "inherited")
        .await?;
    let inherited_seq = started
        .acknowledgement()
        .feed_seq()
        .ok_or("inherited seq")?;
    started
        .into_turn()
        .ok_or("inherited turn")?
        .result()
        .await?;
    let source = session.release().await?;
    let ForkSessionOutcome::Forked { target, .. } = sessions
        .fork(ForkSessionRequest::new(
            SessionOperationId::new("fork:ancestry")?,
            source,
        ))
        .await?
    else {
        return Err("fork".into());
    };
    let common = CommandId::new("root:sibling-local")?;
    let rejected = CommandId::new("rejected:sibling-local")?;
    let mut branches = Vec::new();
    for (label, locator) in [("fork", target), ("source", source)] {
        let OpenSessionOutcome::Opened { session, .. } = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new(format!("open:ancestry:{label}"))?,
                locator,
                binding()?,
            ))
            .await?
        else {
            return Err("open branch".into());
        };
        let inherited_ack = session
            .agent()
            .start(inherited.clone(), "inherited")
            .await?;
        assert_eq!(
            inherited_ack.acknowledgement().feed_seq(),
            Some(inherited_seq)
        );
        assert_eq!(
            inherited_ack.acknowledgement().disposition(),
            crate::agent::CommandAckDisposition::Duplicate
        );
        let local = session
            .agent()
            .start(common.clone(), "same canonical content")
            .await?;
        let local_seq = local.acknowledgement().feed_seq().ok_or("local seq")?;
        local.into_turn().ok_or("local turn")?.result().await?;
        let refusal = session
            .agent()
            .command(crate::agent::TurnCommand::Start {
                command_id: rejected.clone(),
                input: String::new().into(),
            })
            .await?;
        let rejected_seq = refusal.acknowledgement().feed_seq().ok_or("rejected seq")?;
        let rejection = refusal
            .acknowledgement()
            .rejection()
            .ok_or("rejection")?
            .kind();
        branches.push((
            label,
            session.release().await?,
            local_seq,
            rejected_seq,
            rejection,
        ));
    }
    assert_ne!(branches[0].2, branches[1].2);
    assert_ne!(branches[0].3, branches[1].3);
    for (label, locator, local_seq, rejected_seq, rejection) in branches {
        assert!(matches!(
            sessions.prune_history(locator).await?,
            PruneHistoryOutcome::Pruned(_)
        ));
        let OpenSessionOutcome::Opened { session, .. } = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new(format!("reopen:ancestry:{label}"))?,
                locator,
                binding()?,
            ))
            .await?
        else {
            return Err("reopen branch".into());
        };
        for (id, text, seq, kind) in [
            (inherited.clone(), "inherited", inherited_seq, None),
            (common.clone(), "same canonical content", local_seq, None),
            (rejected.clone(), "", rejected_seq, Some(rejection)),
        ] {
            let duplicate = session
                .agent()
                .command(crate::agent::TurnCommand::Start {
                    command_id: id,
                    input: text.to_owned().into(),
                })
                .await?;
            assert_eq!(
                duplicate.acknowledgement().disposition(),
                crate::agent::CommandAckDisposition::Duplicate
            );
            assert_eq!(duplicate.acknowledgement().feed_seq(), Some(seq));
            assert_eq!(
                duplicate
                    .acknowledgement()
                    .rejection()
                    .map(crate::agent::ControlError::kind),
                kind
            );
        }
        session.release().await?;
    }
    Ok(())
}

/// One duplicate submission's disposition, sequence and rejection kind.
async fn resubmit(
    session: &OpenedSession,
    command_id: &CommandId,
    text: &str,
) -> TestResult<(
    crate::agent::CommandAckDisposition,
    Option<plakous_agent::internal::FeedSeq>,
    Option<crate::agent::ControlErrorKind>,
)> {
    let submitted = session
        .agent()
        .command(crate::agent::TurnCommand::Start {
            command_id: command_id.clone(),
            input: text.to_owned().into(),
        })
        .await?;
    let ack = submitted.acknowledgement();
    Ok((
        ack.disposition(),
        ack.feed_seq(),
        ack.rejection().map(crate::agent::ControlError::kind),
    ))
}

#[tokio::test]
async fn command_admission_sequences_follow_rebind_ancestry_not_the_rebound_source() -> TestResult {
    use crate::agent::CommandAckDisposition::{Accepted, Duplicate};
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = MemorySessionStore::with_clock(
        MemoryStoreLimits::default(),
        Arc::new(FixedLeaseClock::new(timestamp()?)),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        Arc::new(store),
        RuntimeDurability::Disabled,
    ))?;
    let source_binding =
        || completing_runtime_binding_for(workspace.clone(), descriptor()?, b"account-alpha");
    let target_descriptor =
        descriptor_for_adapter("target.route", "target.model", "target.adapter")?;
    let target_binding = || {
        completing_runtime_binding_for(
            workspace.clone(),
            target_descriptor.clone(),
            b"account-beta",
        )
    };
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create:rebind-ancestry")?,
            source_binding()?,
        ))
        .await?;
    let inherited = CommandId::new("root:pre-rebind")?;
    let started = session
        .agent()
        .start(inherited.clone(), "inherited")
        .await?;
    let inherited_seq = started.acknowledgement().feed_seq();
    started
        .into_turn()
        .ok_or("inherited turn")?
        .result()
        .await?;
    let rejected = CommandId::new("rejected:pre-rebind")?;
    let (_, rejected_seq, rejection) = resubmit(&session, &rejected, "").await?;
    assert!(rejected_seq.is_some() && rejection.is_some());
    let source = session.release().await?;
    let target_runtime = target_binding()?;
    let selector = target_runtime.target_binding();
    let RebindSessionOutcome::Rebound { target, .. } =
        Box::pin(sessions.rebind(RebindSessionRequest::new(
            SessionOperationId::new("rebind:ancestry")?,
            source,
            selector,
            target_runtime,
        )))
        .await?
    else {
        return Err("rebind".into());
    };

    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:rebind-ancestry:target")?,
            target,
            target_binding()?,
        ))
        .await?
    else {
        return Err("open rebind target".into());
    };
    assert_eq!(
        resubmit(&session, &inherited, "inherited").await?,
        (Duplicate, inherited_seq, None)
    );
    assert_eq!(
        resubmit(&session, &rejected, "").await?,
        (Duplicate, rejected_seq, rejection)
    );
    let local = CommandId::new("root:rebind-local")?;
    let started = session.agent().start(local.clone(), "local").await?;
    let local_seq = started.acknowledgement().feed_seq();
    started.into_turn().ok_or("local turn")?.result().await?;
    let target = session.release().await?;

    // Pruning forces the reopened target to read admissions through the
    // rebind's ancestry rather than its own retained head.
    assert!(matches!(
        sessions.prune_history(target).await?,
        PruneHistoryOutcome::Pruned(_)
    ));
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("reopen:rebind-ancestry:target")?,
            target,
            target_binding()?,
        ))
        .await?
    else {
        return Err("reopen rebind target".into());
    };
    for (id, text, seq, kind) in [
        (&inherited, "inherited", inherited_seq, None),
        (&rejected, "", rejected_seq, rejection),
        (&local, "local", local_seq, None),
    ] {
        assert_eq!(resubmit(&session, id, text).await?, (Duplicate, seq, kind));
    }
    session.release().await?;

    // The rebound source never saw the target's own admission.
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("open:rebind-ancestry:source")?,
            source,
            source_binding()?,
        ))
        .await?
    else {
        return Err("open rebound source".into());
    };
    let started = session.agent().start(local, "local").await?;
    assert_eq!(started.acknowledgement().disposition(), Accepted);
    assert_ne!(started.acknowledgement().feed_seq(), local_seq);
    started.into_turn().ok_or("source turn")?.result().await?;
    session.release().await?;
    Ok(())
}
