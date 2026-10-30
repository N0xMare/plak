#[tokio::test]
async fn sidecar_spawn_refuses_bytes_changed_after_attachment()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("host");
    std::fs::write(&path, b"attached sidecar candidate")?;
    let path = path.canonicalize()?;
    let artifact = LocalCompositionEngineArtifact::quickjs_worker_v1(&path)?;
    let identity = artifact.worker.environment_identity();
    let integration = CellIntegration {
        worker: path.clone(),
        identity,
        presentation: Box::new(JavaScriptPresentation::CodexMinimal {
            nested_catalog: Vec::new(),
        }),
    };
    std::fs::write(path, b"changed sidecar bytes")?;
    let error = integration
        .create_session(1, 10_000, 10_000)
        .await
        .err()
        .ok_or("changed sidecar was spawned")?;
    assert!(error.contains("changed after attachment"), "{error}");
    Ok(())
}
#[test]
fn sidecar_artifact_rejects_relative_paths_and_accepts_canonical_paths()
-> Result<(), Box<dyn std::error::Error>> {
    let relative = Path::new("Cargo.toml");
    let error = LocalCompositionEngineArtifact::quickjs_worker_v1(relative)
        .err()
        .ok_or("relative artifact path was accepted")?;
    assert!(error.to_string().contains("path must already be canonical"));
    let canonical = relative.canonicalize()?;
    let artifact = LocalCompositionEngineArtifact::quickjs_worker_v1(&canonical)?;
    assert_eq!(artifact.path(), canonical);
    Ok(())
}
use super::*;
use plakous_agent::internal::facade_support::StoredCompositionArtifact;

fn recorded(
    artifact: &LocalCompositionEngineArtifact,
) -> Result<StoredCompositionArtifact, AgentError> {
    let host = artifact.worker.environment_identity();
    Ok(StoredCompositionArtifact {
        profile_identity: artifact.engine_contract_identity().identity_bytes(),
        artifact_identity: artifact.identity().identity_bytes(),
        binding_identity: attachment_binding_identity(
            artifact.engine_contract_identity(),
            artifact.identity(),
            artifact.engine_contract_identity(),
            None,
        )?
        .identity_bytes(),
        engine_contract_identity: artifact.engine_contract_identity().identity_bytes(),
        host,
    })
}

#[test]
fn recorded_artifacts_refuse_forged_hash_contract_and_noncanonical_path()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("host");
    std::fs::write(&path, b"offline artifact")?;
    let path = path.canonicalize()?;
    let artifact = LocalCompositionEngineArtifact::quickjs_worker_v1(&path)?;
    let mut exact = recorded(&artifact)?;
    assert_eq!(
        restore_stored_artifact(&exact)?.identity(),
        artifact.identity()
    );
    // Product PTC's evidence suffix is outside the artifact preimage.
    exact
        .host
        .extend_from_slice(&artifact.engine_contract_identity().identity_bytes());
    exact.binding_identity = attachment_binding_identity(
        artifact.engine_contract_identity(),
        artifact.identity(),
        artifact.engine_contract_identity(),
        Some(artifact.engine_contract_identity()),
    )?
    .identity_bytes();
    assert_eq!(
        restore_stored_artifact(&exact)?.identity(),
        artifact.identity()
    );
    exact.engine_contract_identity[0] ^= 1;
    assert!(restore_stored_artifact(&exact).is_err());
    let mut wrong_binding = recorded(&artifact)?;
    wrong_binding.binding_identity[0] ^= 1;
    assert!(restore_stored_artifact(&wrong_binding).is_err());
    let mut forged = recorded(&artifact)?;
    *forged.host.last_mut().ok_or("empty host")? ^= 1;
    assert!(restore_stored_artifact(&forged).is_err());
    let alias = path.parent().ok_or("parent")?.join(".").join("host");
    let original = recorded(&artifact)?;
    let hash = &original.host[original.host.len() - 32..];
    let host = [
        b"rquickjs-host".as_slice(),
        alias.as_os_str().as_encoded_bytes(),
        hash,
    ]
    .concat();
    let noncanonical_artifact =
        digest_environment(b"plakous.local-composition.engine-artifact.v1", &host)?;
    let noncanonical = StoredCompositionArtifact {
        profile_identity: artifact.engine_contract_identity().identity_bytes(),
        artifact_identity: noncanonical_artifact.identity_bytes(),
        binding_identity: attachment_binding_identity(
            artifact.engine_contract_identity(),
            noncanonical_artifact,
            artifact.engine_contract_identity(),
            None,
        )?
        .identity_bytes(),
        engine_contract_identity: artifact.engine_contract_identity().identity_bytes(),
        host,
    };
    assert!(restore_stored_artifact(&noncanonical).is_err());
    {
        let python = LocalCompositionEngineArtifact::monty_worker_v1(&path)?;
        assert_eq!(
            restore_stored_artifact(&recorded(&python)?)?.identity(),
            python.identity()
        );
        let mut disagreement = recorded(&python)?;
        disagreement.engine_contract_identity =
            artifact.engine_contract_identity().identity_bytes();
        disagreement.binding_identity = attachment_binding_identity(
            python.engine_contract_identity(),
            python.identity(),
            artifact.engine_contract_identity(),
            None,
        )?
        .identity_bytes();
        assert!(restore_stored_artifact(&disagreement).is_err());
    }
    Ok(())
}
