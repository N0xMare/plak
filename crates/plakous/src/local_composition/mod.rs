//! Capability-poor attachment for driver-owned local composition.
//!
//! The facade deliberately exposes no session, bridge, responder, executor, or
//! operation trait. Those remain private driver authority:
//!
//! ```compile_fail
//! use plakous::agent::LocalCompositionSession;
//! ```
//!
//! Exact DTO fields cannot be forged by a consumer:
//!
//! ```compile_fail
//! use plakous::agent::LocalCompositionProfile;
//! fn forge() -> LocalCompositionProfile {
//!     LocalCompositionProfile { kind: todo!(), profile_identity: todo!(), engine_contract_identity: todo!() }
//! }
//! ```

#[cfg(feature = "composition-python")]
use std::collections::BTreeMap;
use std::{
    fmt,
    fs::File,
    future::Future,
    io::Read,
    os::unix::ffi::OsStringExt,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use plakous_agent::internal::{
    AgentBuilder, AgentError, AgentErrorKind, LocalCompositionAttachmentIdentity,
    LocalCompositionConstruction, LocalCompositionContentItem, LocalCompositionDescriptor,
    LocalCompositionExecuteInput, LocalCompositionFuture, LocalCompositionImageDetail,
    LocalCompositionIntegration, LocalCompositionLifecycle, LocalCompositionResultProjection,
    LocalCompositionRuntimeContract, LocalCompositionSessionBundle, LocalCompositionShutdownResult,
    LocalCompositionToolDefinition, LocalCompositionWaitInput,
};
use plakous_code_mode as native;
use plakous_compose::{
    CodexMinimalLocalCodeModeDescriptorV1, CodexNativeLocalCodeModeDescriptorV1,
    ProductJavaScriptCompositionDescriptorV1, ProductJavaScriptCompositionV1,
};
#[cfg(feature = "composition-python")]
use plakous_compose::{PythonPtcDescriptorV1, PythonPtcLimitsV1, PythonPtcV1};
use plakous_model::{
    CompiledProviderConfiguration, Digest, DigestDomain, EffectiveProviderConfigurationId,
    IntegrationClass, SelectedProviderConfigurationId, ToolExecutionDisposition, ToolName,
};
use plakous_tools::{
    FrozenToolRegistry, PROCESS_TOOL_PROJECTION_REVISION,
    PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1, PRODUCT_JAVASCRIPT_RUN_NAME_V1,
    PRODUCT_JAVASCRIPT_WAIT_NAME_V1, ProviderModelProjectionIdentityV2, ToolExecutorId,
    ToolProjectionMap,
};
#[cfg(feature = "composition-python")]
use plakous_tools::{
    PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1, PYTHON_PTC_RUN_NAME_V1, PYTHON_PTC_WAIT_NAME_V1,
};

const PRODUCT_JAVASCRIPT_DEFAULT_YIELD_TIME_MS: u64 = 10_000;
const PRODUCT_JAVASCRIPT_DEFAULT_OUTPUT_TOKENS: u64 = 10_000;
#[cfg(feature = "composition-python")]
const PYTHON_DEFAULT_YIELD_TIME_MS: u64 = 10_000;
/// Maximum UTF-8 source bytes in one cell.
#[cfg(feature = "composition-python")]
const PYTHON_SOURCE_MAX_BYTES: usize = 256 * 1024;

mod artifact;
mod attach;
mod evidence;
mod integration;
mod javascript;
#[cfg(feature = "composition-python")]
mod python;
mod session;
mod validate;

pub use artifact::*;
pub use attach::*;
pub use evidence::*;
use integration::{JavaScriptCellIntegration, JavaScriptPresentation, ProductJavaScript};
pub(crate) use javascript::*;
#[cfg(feature = "composition-python")]
use python::{attach_python, neutral_python_descriptor};
use validate::validate_product_configuration;
#[cfg(feature = "composition-python")]
use validate::validate_python_configuration;

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError::new(AgentErrorKind::InvalidInput, message.into())
}

fn identity_error(error: plakous_model::DigestError) -> AgentError {
    AgentError::new(AgentErrorKind::Identity, error.to_string())
}

fn digest_environment(label: &[u8], facts: &[u8]) -> Result<Digest, AgentError> {
    Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[label, facts]).map_err(identity_error)
}

#[cfg(test)]
mod shutdown_tests {
    use super::python::{PythonStructuredRunInput, truncate_python_items};
    use super::{
        Digest, DigestDomain, LocalCompositionContentItem, LocalCompositionEngineArtifact,
        ProductExtensionLimits, attachment_binding_identity, cell_engine_contract_identity,
        monty_engine_contract_identity, product_javascript_limits_are_exact,
        python_extension_limits, validate_engine_contract_binding,
    };

    #[test]
    fn engine_contract_binding_accepts_exact_identity_and_rejects_neighbors() {
        let Ok(exact) = cell_engine_contract_identity(&plakous_cell::QUICKJS_WORKER) else {
            panic!("exact engine identity must construct");
        };
        assert!(validate_engine_contract_binding(exact, exact).is_ok());

        let Ok(neighbor) = Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[b"plakous.local-composition.unqualified-python-neighbor.v1"],
        ) else {
            panic!("neighbor identity must construct");
        };
        let Err(rejected) = validate_engine_contract_binding(exact, neighbor) else {
            panic!("a neighboring engine must not inherit qualification");
        };
        assert!(
            rejected
                .message()
                .contains("engine artifact identities do not match")
        );
    }

    #[test]
    fn python_and_javascript_engine_contracts_are_closed_cross_pairs()
    -> Result<(), Box<dyn std::error::Error>> {
        let codex = cell_engine_contract_identity(&plakous_cell::QUICKJS_WORKER)?;
        let monty = monty_engine_contract_identity()?;
        assert_ne!(codex, monty);
        assert!(validate_engine_contract_binding(monty, monty).is_ok());
        assert!(validate_engine_contract_binding(monty, codex).is_err());
        assert!(validate_engine_contract_binding(codex, monty).is_err());
        Ok(())
    }

    #[test]
    fn monty_artifact_reconstructs_from_exact_held_paths() -> Result<(), Box<dyn std::error::Error>>
    {
        // System true/false may be aliases of the same BusyBox object. Use
        // distinct held files so this test actually exercises artifact drift.
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        let path = root.join("worker");
        std::fs::write(&path, b"original worker bytes")?;
        let artifact = LocalCompositionEngineArtifact::monty_pool_v1(&path)?;
        assert_eq!(artifact.path(), path);
        assert_eq!(
            artifact.engine_contract_identity(),
            monty_engine_contract_identity()?
        );
        assert_ne!(artifact.identity(), artifact.host_identity());

        let drifted_path = root.join("other-worker");
        std::fs::write(&drifted_path, b"different worker bytes")?;
        let drifted = LocalCompositionEngineArtifact::monty_pool_v1(&drifted_path)?;
        assert_ne!(artifact.identity(), drifted.identity());
        assert_ne!(artifact.containment(), drifted.containment());

        // Content replacement at the same canonical path is also identity drift.
        std::fs::write(&path, b"replacement worker bytes")?;
        let replaced = LocalCompositionEngineArtifact::monty_pool_v1(&path)?;
        assert_ne!(artifact.identity(), replaced.identity());
        assert_ne!(artifact.containment(), replaced.containment());
        Ok(())
    }

    #[test]
    fn compiled_python_descriptor_declares_wait_py() -> Result<(), Box<dyn std::error::Error>> {
        let mut registry = plakous_tools::ToolRegistryBuilder::default();
        plakous_tools::register_packs(
            &plakous_compose::standard_coding_packs_in_memory()?,
            &mut registry,
        )?;
        super::PythonPtcV1::register_markers_into(&mut registry)?;
        let configuration = serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"python-descriptor-test"],
        )?)?)?;
        let target = plakous_tools::ProviderModelProjectionIdentityV2::new(
            plakous_model::IntegrationClass::Custom,
            "custom.responses",
            "test.route",
            "test.python",
            "responses-family",
            "test.model",
            "test.model",
            configuration,
            1,
        )?;
        let exact = super::PythonPtcV1::compile(
            registry.freeze(),
            target,
            plakous_compose::PythonPtcPresentationV1::StructuredFunctions,
            [plakous_model::ToolName::new("read_file")?],
        )?;
        let compiled = super::python::neutral_python_descriptor(&exact)?;
        assert_eq!(compiled.execute_name.as_ref(), "exec_py");
        assert_eq!(compiled.wait_name.as_ref(), "wait_py");
        Ok(())
    }

    #[test]
    fn python_limits_and_utf8_byte_truncation_are_exact() {
        assert_eq!(
            python_extension_limits(),
            ProductExtensionLimits {
                max_source_bytes: 262_144,
                max_yield_time_ms: 10_000,
                max_output_tokens: 1_048_576,
            }
        );
        let output = truncate_python_items(
            vec![
                LocalCompositionContentItem::Text("aéz".into()),
                LocalCompositionContentItem::Audio("discarded".into()),
            ],
            2,
        );
        assert_eq!(output, vec![LocalCompositionContentItem::Text("a".into())]);
    }

    #[test]
    fn python_structured_inputs_accept_code_and_byte_bound() {
        let Ok(run): Result<PythonStructuredRunInput, _> =
            serde_json::from_value(serde_json::json!({
                "code": "1 + 1",
                "max_output_bytes": 20_000
            }))
        else {
            panic!("the published run schema must deserialize");
        };
        assert_eq!(run.code, "1 + 1");
        assert_eq!(run.max_output_bytes, Some(20_000.0));
        assert!(
            serde_json::from_value::<PythonStructuredRunInput>(serde_json::json!({
                "code": "1 + 1",
                "yield_time_ms": 9
            }))
            .is_ok(),
            "yield_time_ms is a Python run field"
        );
    }

    #[test]
    fn python_wait_defaults_clamps_and_rejects_invalid_bounds() -> Result<(), String> {
        let wait = super::python::parse_python_wait(serde_json::json!({"cell_id": "2"}))?;
        assert_eq!(wait.yield_time_ms, 10_000);
        assert_eq!(wait.max_output_tokens, 1_048_576);
        assert!(!wait.terminate);
        let wait = super::python::parse_python_wait(
            serde_json::json!({"cell_id": "2", "yield_time_ms": 900_000, "max_output_bytes": 20, "terminate": true}),
        )?;
        assert_eq!(wait.yield_time_ms, 600_000);
        assert_eq!(wait.max_output_tokens, 20);
        assert!(wait.terminate);
        for input in [
            serde_json::json!({"cell_id": ""}),
            serde_json::json!({"cell_id": "2", "yield_time_ms": -1}),
            serde_json::json!({"cell_id": "2", "yield_time_ms": 1.5}),
            serde_json::json!({"cell_id": "2", "max_output_bytes": 0}),
            serde_json::json!({"cell_id": "2", "max_output_bytes": 1_048_577}),
        ] {
            assert!(super::python::parse_python_wait(input).is_err());
        }
        Ok(())
    }

    #[test]
    fn product_javascript_limits_admit_only_the_exact_v1_tuple() {
        let exact = ProductExtensionLimits {
            max_source_bytes: 1_048_576,
            max_yield_time_ms: 10_000,
            max_output_tokens: 10_000,
        };
        assert!(product_javascript_limits_are_exact(exact));
        for neighbor in [
            ProductExtensionLimits {
                max_source_bytes: 0,
                ..exact
            },
            ProductExtensionLimits {
                max_source_bytes: 1_048_577,
                ..exact
            },
            ProductExtensionLimits {
                max_yield_time_ms: 9_999,
                ..exact
            },
            ProductExtensionLimits {
                max_output_tokens: 10_001,
                ..exact
            },
        ] {
            assert!(!product_javascript_limits_are_exact(neighbor));
        }
    }

    #[test]
    fn monty_coroutine_contract_rejects_the_eager_adapter_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let eager = Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[
                b"plakous.monty-python-engine-contract.v1",
                b"monty-pool",
                b"0.0.21",
            ],
        )?;
        let current = monty_engine_contract_identity()?;
        assert_ne!(current, eager);
        assert!(validate_engine_contract_binding(current, eager).is_err());
        assert!(validate_engine_contract_binding(current, current).is_ok());
        Ok(())
    }

    #[test]
    fn native_attachment_identity_bytes_stay_exact_and_product_receipt_rotates_them()
    -> Result<(), Box<dyn std::error::Error>> {
        let profile = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"profile"])?;
        let artifact = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"artifact"])?;
        let engine = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"engine"])?;
        let receipt = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"receipt"])?;
        let expected_native = Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[
                b"plakous.local-composition.attachment.v1",
                &profile.identity_bytes(),
                &artifact.identity_bytes(),
                &engine.identity_bytes(),
            ],
        )?;
        assert_eq!(
            attachment_binding_identity(profile, artifact, engine, None)?,
            expected_native
        );
        assert_ne!(
            attachment_binding_identity(profile, artifact, engine, Some(receipt))?,
            expected_native
        );
        Ok(())
    }
}
