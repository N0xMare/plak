//! The bootstrap crosses the export boundary, never the native codec boundary.

use super::*;
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
};

#[tokio::test]
async fn bootstrap_frozen_r27_presentations_import_through_v13_into_fresh_r28_heads()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bootstrap-v12.json");
    let bundle: serde_json::Value = serde_json::from_slice(&fs::read(fixture)?)?;
    assert_eq!(
        bundle["producer"],
        "9e9785efe0c6549ee8d1c143b81db00472031dab"
    );
    assert_eq!(
        bundle["native_fixture"],
        "a40a209ff426e167583b41debbf7eb8fe63aa564"
    );
    assert_eq!(
        bundle["native_sha256"],
        "fc807cc4a7c4756583eee6b102feb86ab544b49844645344bc74fe980689d900"
    );
    let exports = bundle["exports"].as_array().ok_or("exports")?;
    assert_eq!(exports.len(), 17);
    let root = tempfile::tempdir()?;
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(root.path().join("runtime.sqlite3"))?)?;
    for (index, exported) in exports.iter().enumerate() {
        let value = &exported["document"];
        assert_eq!(value["version"], 12);
        assert_eq!(value["source"]["state_key"], "r27.e37.p37.t10");
        let path = root.path().join("source.json");
        // V12 is no longer an input format. Re-envelope its unchanged neutral
        // payload as v13 for this fixture, without attributing it to a writer.
        let mut portable = value.clone();
        portable["version"] = 13.into();
        portable["kind"] = "portable_session".into();
        portable["source"]["producer"] = serde_json::json!({"status":"unavailable"});
        fs::write(&path, serde_json::to_vec(&portable)?)?;
        let document = read_document(&path)?;
        let old_id = value["source"]["session_id"].as_str().ok_or("source id")?;
        let source: plakous::sessions::SessionLocator = exported["case"]["locator"]
            .as_str()
            .ok_or("source locator")?
            .parse()?;
        let carried = document.into_import()?;
        assert_ne!(carried.record.id, old_id);
        assert_eq!(
            carried.record.carried_forward_from(),
            Some((old_id, "r27.e37.p37.t10"))
        );
        assert_eq!(
            serde_json::to_value(carried.record.composition)?,
            value["binding"]["composition"]
        );
        assert_eq!(
            carried.record.account.as_deref(),
            value["binding"]["account"].as_str()
        );
        assert_eq!(
            carried.record.model.as_deref(),
            value["binding"]["model"].as_str()
        );
        let evidence = root.path().join(format!("evidence-{index}.jsonl"));
        let controller =
            plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                &evidence,
            )?;
        let runtime = plakous_offline_crash_harness::text_runtime_binding(
            root.path(),
            controller,
            "not dispatched",
            "not dispatched",
        )?;
        // S3a deliberately compiles a new binding. This is not an exact reopen
        // of the predecessor's native, API or child presentation.
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(
                CreateSessionRequest::new(
                    SessionOperationId::new(format!("bootstrap:{index}"))?,
                    runtime,
                )
                .with_history(carried.history),
            )
            .await?
        else {
            return Err("bootstrap did not create".into());
        };
        let next = session.release().await?;
        assert_ne!(next.session_id(), source.session_id());
        assert_ne!(next.branch_id(), source.branch_id());
        assert_ne!(next.checkpoint_id(), source.checkpoint_id());
        assert!(
            AgentSessions::inspect_recovery_sqlite(root.path().join("runtime.sqlite3"), next)?
                .is_none()
        );
        assert!(
            fs::read(evidence)?.is_empty(),
            "bootstrap dispatched a model or tool"
        );
    }
    plakous::sessions::rehearsal::audit_runtime_sqlite(&root.path().join("runtime.sqlite3"))?;
    Ok(())
}
