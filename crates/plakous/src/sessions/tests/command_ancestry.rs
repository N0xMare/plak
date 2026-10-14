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
                text: String::new(),
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
                    text: text.to_owned(),
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
