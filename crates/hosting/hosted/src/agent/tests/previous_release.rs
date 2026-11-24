//! A predecessor-native store is refused byte-for-byte after a state-key cut.
use plakous::sessions::SessionBackend;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn previous_epoch_api_checkpoint_reopens_exactly() -> Result<(), Box<dyn std::error::Error>> {
    use crate::agent_tree_protocol::WriterMode;
    use plakous::sessions::{
        AgentSessions, AgentSessionsErrorKind, OpenSessionOutcome, OpenSessionRequest,
        SessionLocator, SessionOperationId,
    };
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    let directory = tempfile::tempdir()?;
    let database = directory.path().join("runtime.sqlite3");
    std::fs::write(
        &database,
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/runtime-epoch-38/runtime.sqlite3"
        )),
    )?;
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600))?;
    let error = SessionBackend::sqlite(&database).expect_err("legacy open must not bootstrap");
    assert_eq!(error.kind(), AgentSessionsErrorKind::BootstrapRequired);
    SessionBackend::bootstrap_sqlite(&database)?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database)?)?;
    let locator: SessionLocator = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/runtime-epoch-38/locator.txt"
    ))
    .trim()
    .parse()?;
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        "current-api",
        "root",
        "owner",
        "/".into(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .map_err(|error| error.message)?;
    let frozen = Arc::new(sessions.created_tool_presentation(locator).await?);
    let extensions =
        crate::topology_extensions::topology_tool_extensions(Arc::new(Mutex::new(tree)), "root")?
            .with_frozen_presentation(Some(frozen));
    let runtime = crate::platform::tests::previous_fixture_runtime(
        &extensions,
        Path::new("/"),
        "current-api",
        true,
        false,
        super::CompositionSelection::Direct,
    )
    .await?;
    let opened = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("previous-epoch:exact-open")?,
            locator,
            runtime,
        ))
        .await?;
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        panic!("previous encoding must reopen under the exact binding");
    };
    assert_eq!(session.release().await?, locator);
    Ok(())
}

#[tokio::test]
async fn current_build_api_presentation_reopens_without_dispatch()
-> Result<(), Box<dyn std::error::Error>> {
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
        OpenSessionRequest, SessionOperationId,
    };
    use std::sync::{Arc, Mutex};
    let directory = tempfile::tempdir()?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(
        directory.path().join("runtime.sqlite3"),
    )?)?;
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        "current-api",
        "root",
        "owner",
        "/".into(),
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .map_err(|error| error.message)?;
    let extensions =
        crate::topology_extensions::topology_tool_extensions(Arc::new(Mutex::new(tree)), "root")?;
    let runtime = crate::platform::tests::previous_fixture_runtime(
        &extensions,
        std::path::Path::new("/"),
        "current-api",
        false,
        false,
        super::CompositionSelection::Direct,
    )
    .await?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("current-api:create")?,
            runtime,
        ))
        .await?
    else {
        panic!("API create")
    };
    let locator = session.release().await?;
    let frozen = Arc::new(sessions.created_tool_presentation(locator).await?);
    assert_ne!(frozen.selected_tools(), []);
    let extensions = extensions.with_frozen_presentation(Some(frozen));
    let runtime = crate::platform::tests::previous_fixture_runtime(
        &extensions,
        std::path::Path::new("/"),
        "current-api",
        true,
        false,
        super::CompositionSelection::Direct,
    )
    .await?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("current-api:open")?,
            locator,
            runtime,
        ))
        .await?
    else {
        panic!("API open")
    };
    assert_eq!(session.release().await?, locator);
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn current_build_production_presentations_reopen_without_dispatch()
-> Result<(), Box<dyn std::error::Error>> {
    use super::{CompositionSelection, ProductStateRoots, production_predecessor};
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
        OpenSessionRequest, SessionOperationId,
    };
    let directory = tempfile::tempdir()?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(
        directory.path().join("runtime.sqlite3"),
    )?)?;
    for composition in [
        CompositionSelection::NativePtc,
        CompositionSelection::JsPtc,
        CompositionSelection::PyPtc,
    ] {
        let authority = format!("current-{}", composition.as_str());
        let roots = || {
            ProductStateRoots::with_doubles(
                directory.path().join(&authority),
                directory.path().join("state"),
            )
        };
        let runtime = production_predecessor::production_predecessor_runtime(
            roots(),
            &authority,
            composition,
            false,
        )
        .await?;
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new(format!("create:{authority}"))?,
                runtime,
            ))
            .await?
        else {
            panic!("current presentation create")
        };
        let locator = session.release().await?;
        let frozen = sessions.created_tool_presentation(locator).await?;
        assert_ne!(frozen.selected_tools(), []);
        assert_eq!(crate::agent::check_frozen_binding(&frozen)?, None);
        let runtime = production_predecessor::production_predecessor_runtime(
            roots(),
            &authority,
            composition,
            true,
        )
        .await?;
        let OpenSessionOutcome::Opened { session, .. } = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new(format!("open:{authority}"))?,
                locator,
                runtime,
            ))
            .await?
        else {
            panic!("current presentation open")
        };
        assert_eq!(session.release().await?, locator);
    }
    Ok(())
}

#[tokio::test]
async fn previous_build_native_store_refuses_a_foreign_state_key_without_rewriting()
-> Result<(), Box<dyn std::error::Error>> {
    let fixtures =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/previous-release");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixtures.join("manifest.json"))?)?;
    assert_ne!(
        manifest["runtime_epoch"],
        plakous::sessions::RUNTIME_FORMAT_EPOCH
    );
    let bytes = std::fs::read(fixtures.join("previous-runtime.sqlite3"))?;
    assert_eq!(
        plakous::model::sha256_hex(&[&bytes]),
        manifest["sha256"].as_str().unwrap()
    );
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("runtime.sqlite3");
    std::fs::write(&database, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600))?;
    }
    let refusal = SessionBackend::sqlite(&database).expect_err("foreign native key must refuse");
    assert!(refusal.to_string().contains("incompatible"), "{refusal}");
    assert!(plakous::sessions::rehearsal::inspect_binding_presentations_sqlite(&database).is_err());
    assert_eq!(std::fs::read(&database)?, bytes);
    Ok(())
}
