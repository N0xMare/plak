//! Engine artifacts: identities, restore from a record and pre-spawn checks.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the local composition module that shares its vocabulary"
)]

use super::*;

/// One verified out-of-process evaluator artifact.
///
/// The value carries only exact artifact facts and a held verified executable.
/// Process launch, bridge supervision, nested dispatch, and cleanup remain in Plakous.
/// Truthful containment facts for one verified evaluator artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalCompositionContainment {
    identity: Digest,
}

impl LocalCompositionContainment {
    /// Returns the semantic identity of these exact containment claims.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.identity
    }

    /// The evaluator is isolated in its own joined process.
    #[must_use]
    pub const fn is_process_isolated(self) -> bool {
        true
    }

    /// The exact host makes no operating-system sandbox claim.
    #[must_use]
    pub const fn claims_os_sandbox(self) -> bool {
        false
    }
}

/// One verified out-of-process evaluator artifact.
///
/// It carries no provider credential, executor, store, or dispatch authority.
pub struct LocalCompositionEngineArtifact {
    pub(super) kind: LocalCompositionEngineArtifactKind,
    pub(super) artifact_identity: Digest,
    pub(super) engine_contract_identity: Digest,
    host_identity: Digest,
    containment: LocalCompositionContainment,
}

pub(super) enum LocalCompositionEngineArtifactKind {
    RquickjsSidecarV1 {
        path: PathBuf,
        environment_identity: Vec<u8>,
    },
    #[cfg(feature = "composition-python")]
    MontyPythonPtcV1 {
        path: PathBuf,
        environment_identity: Vec<u8>,
    },
}

impl LocalCompositionEngineArtifact {
    /// Digest-pinned `monty` worker for product py-ptc.
    ///
    /// # Errors
    ///
    /// Fails when the path is not a readable canonical regular file.
    #[cfg(feature = "composition-python")]
    pub fn monty_pool_v1(worker_path: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let (canonical, _dev, _ino, sha) = file_identity(&worker_path.into())?;
        let environment_identity = [
            b"monty-pool".as_slice(),
            canonical.as_os_str().as_encoded_bytes(),
            &sha,
        ]
        .concat();
        let artifact_identity =
            digest_environment(b"plakous.monty-engine-artifact.v1", &environment_identity)?;
        let host_identity =
            digest_environment(b"plakous.monty-verified-host.v1", &environment_identity)?;
        let containment = LocalCompositionContainment {
            identity: Digest::sha256(
                DigestDomain::ExecutionEnvironment,
                1,
                &[
                    b"plakous.monty-containment.v1",
                    b"sidecar-monty-pool",
                    b"no-os-sandbox-claim",
                    &host_identity.identity_bytes(),
                ],
            )
            .map_err(identity_error)?,
        };
        Ok(Self {
            kind: LocalCompositionEngineArtifactKind::MontyPythonPtcV1 {
                path: canonical,
                environment_identity,
            },
            artifact_identity,
            engine_contract_identity: monty_engine_contract_identity()?,
            host_identity,
            containment,
        })
    }

    /// Records a caller-supplied JavaScript sidecar canonical path and SHA-256.
    ///
    /// The absolute path must already be canonical, with no symlink components.
    /// This records the regular file at attachment, not proof of worker provenance.
    /// The path and hash are re-checked immediately before each evaluator spawn.
    ///
    /// # Errors
    ///
    /// Fails when the path is not canonical or is not a readable regular file.
    pub fn rquickjs_sidecar_v1(host_path: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let (canonical, _dev, _ino, sha) = file_identity(&host_path.into())?;
        let environment_identity = [
            b"rquickjs-host".as_slice(),
            canonical.as_os_str().as_encoded_bytes(),
            &sha,
        ]
        .concat();
        let artifact_identity = digest_environment(
            b"plakous.local-composition.engine-artifact.v1",
            &environment_identity,
        )?;
        let host_identity = digest_environment(
            b"plakous.local-composition.verified-host.v1",
            &environment_identity,
        )?;
        let containment = LocalCompositionContainment {
            identity: Digest::sha256(
                DigestDomain::ExecutionEnvironment,
                1,
                &[
                    b"plakous.local-composition.containment.v1",
                    b"sidecar-rquickjs",
                    b"no-os-sandbox-claim",
                    &host_identity.identity_bytes(),
                ],
            )
            .map_err(identity_error)?,
        };
        Ok(Self {
            kind: LocalCompositionEngineArtifactKind::RquickjsSidecarV1 {
                path: canonical,
                environment_identity,
            },
            artifact_identity,
            engine_contract_identity: rquickjs_sidecar_engine_contract_identity()?,
            host_identity,
            containment,
        })
    }

    /// Returns the exact verified artifact identity, including the executable path and hash.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.artifact_identity
    }

    /// Returns the exact evaluator contract implemented by this artifact.
    #[must_use]
    pub const fn engine_contract_identity(&self) -> Digest {
        self.engine_contract_identity
    }

    /// Returns the verified executable host identity bound separately by extension evidence.
    #[must_use]
    pub const fn host_identity(&self) -> Digest {
        self.host_identity
    }

    /// Returns truthful process-isolation facts without claiming an OS sandbox.
    #[must_use]
    pub const fn containment(&self) -> LocalCompositionContainment {
        self.containment
    }

    /// Returns the verified canonical executable path.
    #[must_use]
    pub fn path(&self) -> &Path {
        match &self.kind {
            LocalCompositionEngineArtifactKind::RquickjsSidecarV1 { path, .. } => path,
            #[cfg(feature = "composition-python")]
            LocalCompositionEngineArtifactKind::MontyPythonPtcV1 { path, .. } => path,
        }
    }
}

impl fmt::Debug for LocalCompositionEngineArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalCompositionEngineArtifact")
            .field("identity", &self.artifact_identity)
            .field("engine_contract_identity", &self.engine_contract_identity)
            .field("path", &self.path())
            .finish_non_exhaustive()
    }
}

pub(super) fn validate_engine_contract_binding(
    profile: Digest,
    artifact: Digest,
) -> Result<(), AgentError> {
    if profile != artifact {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "local composition profile and engine artifact identities do not match",
        ));
    }
    Ok(())
}

pub(super) fn rquickjs_sidecar_engine_contract_identity() -> Result<Digest, AgentError> {
    Digest::sha256(
        DigestDomain::ExecutionEnvironment,
        1,
        &[
            b"plakous.local-composition.product-javascript.v1",
            b"rquickjs-host",
            plakous_js_eval::BOOTSTRAP_REVISION.as_bytes(),
        ],
    )
    .map_err(|error| AgentError::new(AgentErrorKind::Identity, error.to_string()))
}

pub(crate) fn restore_stored_artifact(
    stored: &plakous_agent::internal::facade_support::StoredCompositionArtifact,
) -> Result<LocalCompositionEngineArtifact, AgentError> {
    let refused = || {
        AgentError::new(
            AgentErrorKind::IncompatibleSession,
            "recorded evaluator path, bytes or contract is unavailable or inconsistent; no inference was dispatched",
        )
    };
    let (prefix, label) = if stored.host.starts_with(b"rquickjs-host") {
        (
            b"rquickjs-host".as_slice(),
            b"plakous.local-composition.engine-artifact.v1".as_slice(),
        )
    } else if stored.host.starts_with(b"monty-pool") {
        (
            b"monty-pool".as_slice(),
            b"plakous.monty-engine-artifact.v1".as_slice(),
        )
    } else {
        return Err(refused());
    };
    // Product PTC appends one evidence digest; Code Mode does not. Select the
    // framing by the recorded artifact digest, never by probing candidate files.
    let evidence_len = rquickjs_sidecar_engine_contract_identity()?
        .identity_bytes()
        .len();
    let host = [
        Some(stored.host.as_slice()),
        stored
            .host
            .len()
            .checked_sub(evidence_len)
            .map(|len| &stored.host[..len]),
    ]
    .into_iter()
    .flatten()
    .find(|host| {
        digest_environment(label, host)
            .is_ok_and(|digest| digest.identity_bytes() == stored.artifact_identity)
    })
    .ok_or_else(refused)?;
    let mut binding_parts = vec![
        b"plakous.local-composition.attachment.v1".as_slice(),
        stored.profile_identity.as_slice(),
        stored.artifact_identity.as_slice(),
        stored.engine_contract_identity.as_slice(),
    ];
    if host.len() != stored.host.len() {
        let receipt = &stored.host[host.len()..];
        let digest = Digest::from_sha256_bytes(
            DigestDomain::ExecutionEnvironment,
            1,
            receipt[receipt.len() - 32..]
                .try_into()
                .map_err(|_| refused())?,
        )
        .map_err(|_| refused())?;
        if digest.identity_bytes() != receipt {
            return Err(refused());
        }
        binding_parts.push(receipt);
    }
    if Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &binding_parts)
        .map_err(|_| refused())?
        .identity_bytes()
        != stored.binding_identity
    {
        return Err(refused());
    }
    let path_end = host
        .len()
        .checked_sub(32)
        .filter(|end| *end > prefix.len())
        .ok_or_else(refused)?;
    let path = PathBuf::from(std::ffi::OsString::from_vec(
        host[prefix.len()..path_end].to_vec(),
    ));
    if !path.is_absolute()
        || std::fs::canonicalize(&path)
            .map_err(|_| refused())?
            .as_os_str()
            != path.as_os_str()
    {
        return Err(refused());
    }
    let artifact = if prefix == b"rquickjs-host" {
        LocalCompositionEngineArtifact::rquickjs_sidecar_v1(&path).map_err(|_| refused())?
    } else {
        #[cfg(feature = "composition-python")]
        {
            LocalCompositionEngineArtifact::monty_pool_v1(&path).map_err(|_| refused())?
        }
        #[cfg(not(feature = "composition-python"))]
        {
            return Err(refused());
        }
    };
    if artifact.identity().identity_bytes() != stored.artifact_identity
        || artifact.engine_contract_identity().identity_bytes() != stored.engine_contract_identity
    {
        return Err(refused());
    }
    Ok(artifact)
}

#[cfg(feature = "composition-python")]
pub(super) fn monty_engine_contract_identity() -> Result<Digest, AgentError> {
    Digest::sha256(
        DigestDomain::ExecutionEnvironment,
        1,
        &[
            b"plakous.monty-python-engine-contract.v2",
            b"monty-pool",
            b"1.2.0",
            crate::monty_python::ADAPTER_REVISION.as_bytes(),
        ],
    )
    .map_err(identity_error)
}

pub(crate) fn verify_sidecar_before_spawn(path: &Path, expected: &[u8]) -> Result<(), String> {
    let artifact = LocalCompositionEngineArtifact::rquickjs_sidecar_v1(path)
        .map_err(|error| error.to_string())?;
    match artifact.kind {
        LocalCompositionEngineArtifactKind::RquickjsSidecarV1 {
            environment_identity,
            ..
        } if environment_identity == expected => Ok(()),
        _ => Err("JavaScript sidecar identity changed after attachment".to_owned()),
    }
}

fn file_identity(path: &Path) -> Result<(PathBuf, u64, u64, [u8; 32]), AgentError> {
    use sha2::Digest as _;
    let canonical = path
        .canonicalize()
        .map_err(|error| invalid(error.to_string()))?;
    if canonical != path {
        return Err(invalid(
            "composition executable path must already be canonical",
        ));
    }
    let mut file = File::open(&canonical).map_err(|error| invalid(error.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|error| invalid(error.to_string()))?;
    if !metadata.is_file() {
        return Err(invalid("composition executable must be a regular file"));
    }
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| invalid(error.to_string()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok((
        canonical,
        metadata.dev(),
        metadata.ino(),
        hasher.finalize().into(),
    ))
}

#[cfg(test)]
mod artifact_recovery_tests {
    #[tokio::test]
    async fn sidecar_spawn_refuses_bytes_changed_after_attachment()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("host");
        std::fs::write(&path, b"attached sidecar candidate")?;
        let path = path.canonicalize()?;
        let artifact = LocalCompositionEngineArtifact::rquickjs_sidecar_v1(&path)?;
        let identity = match artifact.kind {
            LocalCompositionEngineArtifactKind::RquickjsSidecarV1 {
                environment_identity,
                ..
            } => environment_identity,
            #[cfg(feature = "composition-python")]
            LocalCompositionEngineArtifactKind::MontyPythonPtcV1 { .. } => {
                unreachable!("constructor always produces a JavaScript artifact")
            }
        };
        let integration = crate::js_code_mode::RquickjsCodeModeIntegration {
            identity,
            profile: CodexIntegrationProfile::MinimalCore,
            nested_catalog: Vec::new(),
            sidecar: path.clone(),
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
        let error = LocalCompositionEngineArtifact::rquickjs_sidecar_v1(relative)
            .err()
            .ok_or("relative artifact path was accepted")?;
        assert!(error.to_string().contains("path must already be canonical"));
        let canonical = relative.canonicalize()?;
        let artifact = LocalCompositionEngineArtifact::rquickjs_sidecar_v1(&canonical)?;
        assert_eq!(artifact.path(), canonical);
        Ok(())
    }
    use super::*;
    use plakous_agent::internal::facade_support::StoredCompositionArtifact;

    fn recorded(
        artifact: &LocalCompositionEngineArtifact,
    ) -> Result<StoredCompositionArtifact, AgentError> {
        let host = match &artifact.kind {
            LocalCompositionEngineArtifactKind::RquickjsSidecarV1 {
                environment_identity,
                ..
            } => environment_identity.clone(),
            #[cfg(feature = "composition-python")]
            LocalCompositionEngineArtifactKind::MontyPythonPtcV1 {
                environment_identity,
                ..
            } => environment_identity.clone(),
        };
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
        let artifact = LocalCompositionEngineArtifact::rquickjs_sidecar_v1(&path)?;
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
        #[cfg(feature = "composition-python")]
        {
            let python = LocalCompositionEngineArtifact::monty_pool_v1(&path)?;
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
}

#[cfg(all(test, feature = "composition-python"))]
mod monty_upgrade_tests {
    use super::*;

    /// The engine contract this build's predecessor recorded: Monty 1.1.0
    /// under adapter revision v6.
    fn previous_monty_contract() -> Result<Digest, AgentError> {
        Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[
                b"plakous.monty-python-engine-contract.v2",
                b"monty-pool",
                b"1.1.0",
                b"plakous.monty-python-adapter.v6",
            ],
        )
        .map_err(identity_error)
    }

    #[test]
    fn the_contract_names_monty_1_2_0_and_adapter_v7() -> Result<(), AgentError> {
        assert_eq!(
            crate::monty_python::ADAPTER_REVISION,
            "plakous.monty-python-adapter.v7"
        );
        let expected = Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[
                b"plakous.monty-python-engine-contract.v2",
                b"monty-pool",
                b"1.2.0",
                b"plakous.monty-python-adapter.v7",
            ],
        )
        .map_err(identity_error)?;
        assert_eq!(monty_engine_contract_identity()?, expected);
        Ok(())
    }

    #[test]
    fn a_py_ptc_record_under_the_previous_contract_is_a_typed_refusal()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("monty");
        std::fs::write(&path, b"offline worker")?;
        let artifact = LocalCompositionEngineArtifact::monty_pool_v1(path.canonicalize()?)?;
        let LocalCompositionEngineArtifactKind::MontyPythonPtcV1 {
            environment_identity,
            ..
        } = &artifact.kind
        else {
            return Err("monty artifact".into());
        };
        let previous = previous_monty_contract()?;
        assert_ne!(previous, artifact.engine_contract_identity());
        // A self-consistent record of the same worker bytes, bound under the
        // previous contract: only the contract disagrees with this build.
        let stored = plakous_agent::internal::facade_support::StoredCompositionArtifact {
            profile_identity: previous.identity_bytes(),
            artifact_identity: artifact.identity().identity_bytes(),
            binding_identity: attachment_binding_identity(
                previous,
                artifact.identity(),
                previous,
                None,
            )?
            .identity_bytes(),
            engine_contract_identity: previous.identity_bytes(),
            host: environment_identity.clone(),
        };
        let Err(refusal) = restore_stored_artifact(&stored) else {
            return Err("a previous-contract record must not reopen".into());
        };
        assert_eq!(refusal.kind(), AgentErrorKind::IncompatibleSession);
        assert!(!refusal.message().contains("plak session"));
        assert!(
            refusal.message().contains("no inference was dispatched"),
            "{}",
            refusal.message()
        );
        Ok(())
    }
}
