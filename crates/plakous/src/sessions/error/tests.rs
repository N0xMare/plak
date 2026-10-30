use super::*;

/// A store refused for a permanent property of its path names the path and
/// a remedy instead of reading as a store that is busy.
#[cfg(feature = "sessions-sqlite")]
#[test]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "test fixtures fail at exact lines"
)]
fn unsafe_store_paths_name_the_path_and_the_remedy() {
    use crate::sessions::SessionBackend;
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().expect("temporary directory");
    let real = directory.path().join("real");
    std::fs::create_dir(&real).expect("real directory");
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).expect("symlinked ancestor");

    let Err(error) = SessionBackend::sqlite(alias.join("runtime.sqlite3")) else {
        panic!("a symlinked ancestor must be refused");
    };
    assert_eq!(error.kind(), AgentSessionsErrorKind::UnsafeStorePath);
    let message = error.to_string();
    assert!(
        message.contains(&format!("{} is a symbolic link", alias.display())),
        "{message}"
    );
    assert!(message.contains("no symbolic link"), "{message}");

    let database = real.join("runtime.sqlite3");
    drop(SessionBackend::sqlite(&database).expect("private store"));
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644))
        .expect("world-readable store");
    let Err(error) = SessionBackend::classify_sqlite(&database) else {
        panic!("a world-readable store must be refused");
    };
    assert_eq!(error.kind(), AgentSessionsErrorKind::UnsafeStorePath);
    assert!(error.to_string().contains("chmod 600"), "{error}");

    // A busy refusal still reads as busy, never as the path.
    assert_eq!(
        store_path_error(&real, SessionStoreError::BackendBusy).kind(),
        AgentSessionsErrorKind::StorageBusy
    );
}

#[test]
fn workflow_unknown_is_not_an_ownership_refusal() {
    let error = runtime_error(&AgentError::new(
        AgentErrorKind::OutcomeUnknown,
        "outcome unknown",
    ));
    assert_eq!(error.kind(), AgentSessionsErrorKind::OutcomeUnknown);
}

#[test]
fn ownership_refusal_names_the_busy_session_and_lease() {
    let error = store_origin_error(SessionStoreOrigin::Ownership);
    assert_eq!(error.kind(), AgentSessionsErrorKind::Ownership);
    assert!(error.to_string().contains("session is busy"));
    assert!(error.to_string().contains("lease is held by another owner"));
    assert!(!error.to_string().contains("RecoveryRequired"));
}

#[test]
fn reconstruction_diagnostics_are_specific_but_never_echo_unknown_text() {
    let known = runtime_error(&AgentError::new(
        AgentErrorKind::IncompatibleSession,
        "builder semantics disagree with the durable checkpoint",
    ));
    assert!(
        known
            .to_string()
            .ends_with("(reconstruction: stable request prefix)")
    );
    let named = runtime_error(&AgentError::new(
        AgentErrorKind::IncompatibleSession,
        "builder semantics disagree with the durable checkpoint \
             (stable-prefix fields: instructions,tool-choice)",
    ));
    assert!(
        named
            .to_string()
            .ends_with("(reconstruction: stable request prefix: instructions,tool-choice)")
    );
    let forged = runtime_error(&AgentError::new(
        AgentErrorKind::IncompatibleSession,
        "builder semantics disagree with the durable checkpoint \
             (stable-prefix fields: secret text)",
    ));
    assert!(
        forged
            .to_string()
            .ends_with("(reconstruction: runtime reconstruction)")
    );
    let unknown = runtime_error(&AgentError::new(
        AgentErrorKind::IncompatibleSession,
        "private executor details must not escape",
    ));
    assert!(
        unknown
            .to_string()
            .ends_with("(reconstruction: runtime reconstruction)")
    );
    assert!(!unknown.to_string().contains("private executor"));
}
