use super::*;

fn reserve(
    registry: &TerminalRegistry,
    count: &AttachmentCount,
    runner: &str,
    principal: &str,
    session: &str,
    number: usize,
) -> Result<String, TerminalError> {
    let mut terminal = pin(registry, registry.paths.home().to_owned());
    terminal.runner = runner.into();
    terminal.namespace = registry.namespace(&context(principal).principal).unwrap();
    terminal.key.session = id(session.into()).unwrap();
    terminal.key.terminal_id = id(format!("terminal-{number}")).unwrap();
    registry.reserve(terminal, count.hold().unwrap())
}

#[test]
fn operator_host_collects_over_a_thousand_receipts_without_global_open_exhaustion() {
    let (_home, registry, count) = fixture();
    for number in 0..1300 {
        let identity = reserve(&registry, &count, "local", "operator", "session", number).unwrap();
        registry.release_proven(&identity).unwrap();
    }
    assert_eq!(registry.lock().entries.len(), 8);
    assert_eq!(
        registry
            .lock()
            .fences
            .values()
            .map(BTreeMap::len)
            .sum::<usize>(),
        1292
    );
    assert!(matches!(
        reserve(&registry, &count, "local", "operator", "session", 0),
        Err(TerminalError::Lost)
    ));
    reserve(
        &registry,
        &count,
        "other-runner",
        "other",
        "other-session",
        1301,
    )
    .unwrap();
    assert_eq!(count.get(), 1);
    assert!(!registry.paths.product_database().exists());
}

#[test]
fn operator_host_session_principal_and_host_quotas_span_runners() {
    let (_home, registry, count) = fixture();
    reserve(&registry, &count, "one", "operator", "session", 0).unwrap();
    reserve(&registry, &count, "two", "operator", "session", 1).unwrap();
    assert!(matches!(
        reserve(&registry, &count, "three", "operator", "session", 2),
        Err(TerminalError::Refused(OperatorRefusal::Quota))
    ));
    reserve(&registry, &count, "one", "operator", "second", 3).unwrap();
    reserve(&registry, &count, "two", "operator", "second", 4).unwrap();
    assert!(matches!(
        reserve(&registry, &count, "three", "operator", "third", 5),
        Err(TerminalError::Refused(OperatorRefusal::Quota))
    ));
    for number in 4..16 {
        reserve(
            &registry,
            &count,
            &format!("runner-{}", number % 2),
            &format!("principal-{}", number / 4),
            &format!("session-{number}"),
            number,
        )
        .unwrap();
    }
    assert!(matches!(
        reserve(&registry, &count, "new-runner", "new", "new", 16),
        Err(TerminalError::Refused(OperatorRefusal::Quota))
    ));
    assert_eq!(count.get(), 16);
}

#[test]
fn operator_host_open_rate_is_six_per_principal_minute_and_refills() {
    let (_home, registry, _count) = fixture();
    let namespace = registry.namespace(&context("operator").principal).unwrap();
    for _ in 0..6 {
        registry.admit_open(&namespace).unwrap();
    }
    assert!(matches!(
        registry.admit_open(&namespace),
        Err(TerminalError::Refused(OperatorRefusal::Backpressure))
    ));
    let other = registry.namespace(&context("other").principal).unwrap();
    registry.admit_open(&other).unwrap();
    let expired = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(61))
        .unwrap();
    for time in registry
        .lock()
        .opened
        .get_mut(namespace.principal.as_str())
        .unwrap()
    {
        *time = expired;
    }
    registry.admit_open(&namespace).unwrap();
}

#[tokio::test]
async fn operator_host_operation_locks_isolate_runners_and_principal_namespaces() {
    let (_home, registry, _count) = fixture();
    let namespace = registry.namespace(&context("operator").principal).unwrap();
    let other = registry.namespace(&context("other").principal).unwrap();
    let _stalled = registry.runner_operation("stalled").write_owned().await;
    let _first = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        registry.namespace_operation("healthy", &namespace),
    )
    .await
    .unwrap();
    let _second = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        registry.namespace_operation("healthy", &other),
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            registry.namespace_operation("healthy", &namespace)
        )
        .await
        .is_err()
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            registry.namespace_operation("stalled", &namespace)
        )
        .await
        .is_err()
    );
}
