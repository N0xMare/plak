use super::*;

#[test]
fn reopen_normalizes_a_nested_repeated_chain_before_cleanup_is_added() {
    let chain = "initiating Session failure: session store disk is full; cleanup disposition: initiating Session failure: session store disk is full";
    let message = format!(
        "reopen failed: {chain}; cleanup disposition: {chain}; {chain}; remedy: free disk space and retry"
    );
    let failure = CliFailure::recovery_required(message).with_context("resume");
    assert_eq!(
        failure
            .message()
            .matches("session store disk is full")
            .count(),
        1
    );
    assert_eq!(
        failure
            .message()
            .matches("cleanup disposition: initiating Session failure: [same cause]")
            .count(),
        1
    );
    assert!(
        failure
            .message()
            .ends_with("remedy: free disk space and retry")
    );
    assert_eq!(
        collapse_repeated_causes(failure.message()),
        failure.message()
    );
    assert_eq!(failure.exit_code(), 5);
}

#[test]
fn repeated_causes_keep_all_stages_and_the_failure_category() {
    let failure = CliFailure::classified(
        CliFailureKind::RecoveryRequired,
        "session could not be reopened: session store disk is full",
    ).with_cleanup_failure("cleanup disposition: initiating Session failure: session store disk is full; cleanup disposition: session store disk is full; telemetry: writer stopped");
    assert_eq!(
        failure
            .message()
            .matches("session store disk is full")
            .count(),
        1
    );
    assert_eq!(failure.message().matches("[same cause]").count(), 2);
    assert!(failure.message().contains("initiating Session failure:"));
    assert!(failure.message().contains("telemetry: writer stopped"));
    assert_eq!(failure.kind, CliFailureKind::RecoveryRequired);
    assert_eq!(failure.exit_code(), 5);
}

#[test]
fn distinct_causes_and_nested_context_are_not_dropped() {
    let values = "expected revision: 3; actual revision: 3";
    assert_eq!(CliFailure::new(values).message(), values);
    assert_eq!(CliFailure::recovery_required(values).message(), values);
    let message = "open: database full; cleanup: permission denied; release: lease lost";
    assert_eq!(CliFailure::new(message).message(), message);
    let failure =
        CliFailure::new("open: database full").with_cleanup_failure("release: database full");
    assert_eq!(
        failure.message(),
        "open: database full; release: [same cause]"
    );
}

#[test]
fn compressed_collisions_preserve_distinct_original_stages() {
    for (message, expected) in [
        (
            "open: disk full; release: lease lost; cleanup disposition: disk full; cleanup disposition: lease lost",
            "open: disk full; release: lease lost; cleanup disposition: [same cause]; cleanup disposition: lease lost",
        ),
        (
            "initiating Session failure: disk full; cleanup disposition: lease lost; disk full; lease lost",
            "initiating Session failure: disk full; cleanup disposition: lease lost; [same cause]; lease lost",
        ),
    ] {
        assert_eq!(collapse_repeated_causes(message), expected);
        assert_eq!(collapse_repeated_causes(expected), expected);
    }
}

#[test]
fn shutdown_keeps_release_and_cleanup_failures_typed() {
    let failure = CliFailure::shutdown(
        Some(CliFailure::classified(CliFailureKind::Agent, "canonical")),
        Some(CliFailure::new("cleanup")),
    )
    .unwrap_err();
    let parts = failure.shutdown_failures().unwrap();
    assert_eq!(parts.release.as_ref().unwrap().kind, CliFailureKind::Agent);
    assert_eq!(parts.cleanup.as_ref().unwrap().message(), "cleanup");
    assert!(failure.message().contains("cleanup") && failure.message().contains("canonical"));
}

#[test]
fn context_preserves_the_typed_session_error() {
    let source = plakous::sessions::SessionOperationId::new("").unwrap_err();
    let failure = CliFailure::from(source.clone()).with_context("context meter after recovery");
    let retained = std::error::Error::source(&failure)
        .unwrap()
        .downcast_ref::<plakous::sessions::AgentSessionsError>()
        .unwrap();
    assert_eq!(retained, &source);
    assert!(
        failure
            .message()
            .starts_with("context meter after recovery:")
    );
}

#[test]
fn recovery_required_is_distinct_from_agent_kind() {
    let failure = CliFailure::recovery_required("orchestration snapshot digest mismatch");
    assert_eq!(failure.kind, CliFailureKind::RecoveryRequired);
    assert_eq!(failure.exit_code(), 5);
    assert!(failure.is_recovery_required());
    assert!(
        !CliFailure::classified(CliFailureKind::Agent, "agent run failed").is_recovery_required()
    );
    assert!(!CliFailure::new("internal").is_recovery_required());
}
