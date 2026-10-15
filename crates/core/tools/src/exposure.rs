//! Frozen provider-facing aliases over exact registered tool identities.

mod lowering;
mod map;
mod projection;
mod raising;

pub use lowering::{PI_READ_IMAGE_REFUSAL, UNIFIED_EXEC_MAX_YIELD_MS, UNIFIED_EXEC_MIN_YIELD_MS};
pub use map::{ToolExposure, ToolExposureMap};
pub(crate) use map::{
    exposed_definition, exposure_map_digest, registry_identity_digest,
    registry_identity_digest_from_identities,
};
pub(crate) use projection::projection_map_digest;
pub use projection::{
    CanonicalProcessOperationV2, ProjectedToolInvocationV2, ProviderModelProjectionIdentityV2,
    ToolProjectionExposureV2, ToolProjectionLoweringV2, ToolProjectionMap, ToolProjectionRaisingV2,
    ToolProjectionReceiptV2,
};

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::path::Path;
    use std::str::FromStr;
    use std::sync::Arc;

    use plakous_model::{
        Digest, DigestDomain, IntegrationClass, RunId, SemanticJsonV1, ToolConcurrency,
        ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
        ToolResultPart,
    };
    use serde_json::{Value, json};

    use super::lowering::{canonical_process_workdir, unified_exec_yield_time_ms};
    use super::projection::missing_required_fields;
    use super::*;
    use crate::{
        ExecutionEnvironment, FrozenToolRegistry, PortableToolSchemaV2, RegisteredToolId,
        ToolExecutorId, ToolOutcome, ToolRegistration, ToolRegistryBuilder, ToolRuntimeErrorKind,
    };

    #[test]
    fn unified_exec_yield_clamps_long_requests_and_zero() {
        for (requested, expected) in [(120_000, 120_000), (900_000, 600_000), (0, 250)] {
            let arguments = json!({"yield_time_ms": requested});
            assert_eq!(
                unified_exec_yield_time_ms(arguments.as_object().unwrap()),
                expected
            );
        }
    }

    #[test]
    fn missing_required_fields_names_only_absent_schema_keys() {
        let schema = PortableToolSchemaV2::new(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "template": {"type": "string"},
                "prompt": {"type": "string"},
                "output_schema_json": {"type": "string"},
                "tier": {"type": "string"}
            },
            "required": ["template", "prompt", "output_schema_json", "tier"],
            "additionalProperties": false
        }))
        .unwrap();

        assert_eq!(
            missing_required_fields(
                &schema,
                r#"{"template":"review","tier":"read_only","secret":"not echoed"}"#,
            )
            .as_deref(),
            Some("prompt, output_schema_json")
        );
        assert_eq!(missing_required_fields(&schema, "not json"), None);
    }

    fn registry_with_output(
        output: ToolOutputContract,
    ) -> Result<FrozenToolRegistry, Box<dyn std::error::Error>> {
        let definition = ToolDefinition::new(
            ToolName::new("read")?,
            "1.0.0",
            "SENSITIVE_DESCRIPTION_SENTINEL",
            ToolInputContract::structured(
                json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "properties":{"SENSITIVE_SCHEMA_SENTINEL":{"type":"string"}},"required":["SENSITIVE_SCHEMA_SENTINEL"],"additionalProperties":false}),
            )?,
            output,
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?;
        let tool = Arc::new(crate::test_support::FixedTool::new(
            ToolOutcome::success_text("ok")?,
        ));
        let mut builder = ToolRegistryBuilder::default();
        builder.register(ToolRegistration::new(
            RegisteredToolId::new("read-id")?,
            ToolExecutorId::new("read-executor")?,
            crate::ToolExecutorBindingIdentity::new(1, &[b"read-executor-binding"])?,
            definition,
            tool,
        ))?;
        Ok(builder.freeze())
    }

    fn registry() -> Result<FrozenToolRegistry, Box<dyn std::error::Error>> {
        registry_with_output(ToolOutputContract::portable_model_content())
    }

    fn projection_target() -> Result<ProviderModelProjectionIdentityV2, Box<dyn std::error::Error>>
    {
        let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"projection-test-effective-configuration"],
        )?)?)?;
        Ok(ProviderModelProjectionIdentityV2::new(
            IntegrationClass::Custom,
            "custom.protocol",
            "custom.route",
            "custom.profile",
            "custom-family",
            "custom-model",
            "custom-model",
            effective,
            1,
        )?)
    }

    #[test]
    fn codex_patch_lowering_rejects_a_structured_canonical_target_at_freeze()
    -> Result<(), Box<dyn std::error::Error>> {
        let registry = registry()?;
        let identity = registry
            .identity(&ToolName::new("read")?)
            .ok_or("test canonical tool missing")?
            .clone();
        let target = projection_target()?;
        let entry = ToolProjectionExposureV2::new(
            [(CanonicalProcessOperationV2::Poll, identity)],
            "apply_patch",
            "Patch raw text",
            PortableToolSchemaV2::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"patch":{"type":"string"}},
                "required":["patch"],
                "additionalProperties":false
            }))?,
            SemanticJsonV1::from_value(json!({}))?,
            ToolProjectionLoweringV2::CodexApplyPatch,
            PortableToolSchemaV2::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{},
                "required":[],
                "additionalProperties":false
            }))?,
            ToolProjectionRaisingV2::Identity,
            target.clone(),
        )?;
        assert_eq!(
            ToolProjectionMap::new(registry, target, [entry])
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::Registration)
        );
        Ok(())
    }

    fn text_projection(
        raising: ToolProjectionRaisingV2,
    ) -> Result<(ToolProjectionMap, ToolProjectionReceiptV2), Box<dyn std::error::Error>> {
        text_projection_with_output(raising, ToolOutputContract::portable_model_content())
    }

    fn text_projection_with_output(
        raising: ToolProjectionRaisingV2,
        output: ToolOutputContract,
    ) -> Result<(ToolProjectionMap, ToolProjectionReceiptV2), Box<dyn std::error::Error>> {
        let registry = registry_with_output(output)?;
        let identity = registry
            .identity(&ToolName::new("read")?)
            .ok_or("registered identity is missing")?
            .clone();
        let target = projection_target()?;
        let entry = ToolProjectionExposureV2::new(
            [(CanonicalProcessOperationV2::Poll, identity.clone())],
            "read_file",
            "Read one value.",
            PortableToolSchemaV2::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"SENSITIVE_SCHEMA_SENTINEL":{"type":"string"}},
                "required":["SENSITIVE_SCHEMA_SENTINEL"],
                "additionalProperties":false
            }))?,
            SemanticJsonV1::from_value(json!({}))?,
            ToolProjectionLoweringV2::CanonicalPassthrough,
            PortableToolSchemaV2::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"result":{"type":"string"}},
                "required":["result"],
                "additionalProperties":false
            }))?,
            raising,
            target.clone(),
        )?;
        let receipt = ToolProjectionReceiptV2 {
            projection_identity: entry.identity,
            exposure_map_identity: Digest::sha256(DigestDomain::RequestPlan, 1, &[b"placeholder"])?,
            canonical_identity: identity,
            canonical_operation: CanonicalProcessOperationV2::Poll,
            exposed_name: ToolName::new("read_file")?,
            lowering_identity: entry.lowering_identity,
            raising_identity: entry.raising_identity,
            target: target.clone(),
        };
        let map = ToolProjectionMap::new(registry, target, [entry])?;
        let receipt = ToolProjectionReceiptV2 {
            exposure_map_identity: map.digest(),
            ..receipt
        };
        Ok((map, receipt))
    }

    #[test]
    fn identity_raising_validates_and_text_raising_matches_its_schema()
    -> Result<(), Box<dyn std::error::Error>> {
        let (identity, receipt) = text_projection(ToolProjectionRaisingV2::Identity)?;
        assert!(
            identity
                .raise(&receipt, ToolOutcome::success_text("plain text")?)
                .is_err()
        );

        let (wrapped, receipt) = text_projection(ToolProjectionRaisingV2::TextResultObjectV1)?;
        let outcome = wrapped.raise(&receipt, ToolOutcome::success_text("plain text")?)?;
        let [ToolResultPart::Text(text)] = outcome.parts() else {
            return Err("raised result was not text".into());
        };
        assert_eq!(
            serde_json::from_str::<Value>(text.text())?,
            json!({
                "result": "plain text"
            })
        );

        let (raw, receipt) = text_projection(ToolProjectionRaisingV2::RawUtf8V1)?;
        let outcome = raw.raise(&receipt, ToolOutcome::success_text("plain text")?)?;
        let [ToolResultPart::Text(text)] = outcome.parts() else {
            return Err("raw utf8 result was not text".into());
        };
        assert_eq!(text.text(), "plain text");
        Ok(())
    }

    #[test]
    fn text_result_object_raises_every_result_its_canonical_contract_permits()
    -> Result<(), Box<dyn std::error::Error>> {
        let (wrapped, receipt) = text_projection(ToolProjectionRaisingV2::TextResultObjectV1)?;
        // The largest canonical texts the default 1 MiB outcome bound admits:
        // plain bytes, and quotes that the wrapper escapes once more.
        for body in ["x".repeat(1_048_000), "\"".repeat(524_000)] {
            let canonical = ToolOutcome::success_text(body.clone())?;
            let outcome = wrapped.raise(&receipt, canonical)?;
            let [ToolResultPart::Text(text)] = outcome.parts() else {
                return Err("raised result was not text".into());
            };
            assert!(text.text().len() > SemanticJsonV1::maximum_string_bytes());
            assert_eq!(
                serde_json::from_str::<Value>(text.text())?,
                json!({ "result": body })
            );
        }
        Ok(())
    }

    #[test]
    fn text_result_raising_refuses_a_schema_that_does_not_admit_every_text_result()
    -> Result<(), Box<dyn std::error::Error>> {
        let registry = registry_with_output(ToolOutputContract::portable_model_content())?;
        let identity = registry
            .identity(&ToolName::new("read")?)
            .ok_or("registered identity is missing")?
            .clone();
        let input = PortableToolSchemaV2::new(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{},
            "required":[],
            "additionalProperties":false
        }))?;
        for properties in [
            json!({"result": {"type": "string"}, "extra": {"type": "string"}}),
            json!({"result": {"type": "string", "enum": ["only"]}}),
            json!({"result": {"type": "integer"}}),
        ] {
            let output = PortableToolSchemaV2::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties": properties,
                "required":["result"],
                "additionalProperties":false
            }))?;
            assert!(
                ToolProjectionExposureV2::new(
                    [(CanonicalProcessOperationV2::Poll, identity.clone())],
                    "read_file",
                    "Read one value.",
                    input.clone(),
                    SemanticJsonV1::from_value(json!({}))?,
                    ToolProjectionLoweringV2::CanonicalPassthrough,
                    output,
                    ToolProjectionRaisingV2::TextResultObjectV1,
                    projection_target()?,
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn model_content_identity_validates_the_canonical_declared_contract()
    -> Result<(), Box<dyn std::error::Error>> {
        let contract = ToolOutputContract::model_content(2, 256)?;
        let (projection, receipt) =
            text_projection_with_output(ToolProjectionRaisingV2::ModelContentIdentityV1, contract)?;
        let outcome = ToolOutcome::success(vec![
            ToolResultPart::Text(plakous_model::TextBlock::new("first", None)?),
            ToolResultPart::Text(plakous_model::TextBlock::new("second", None)?),
        ])?;
        assert_eq!(projection.raise(&receipt, outcome.clone())?, outcome);

        let oversized = ToolOutcome::success_text("x".repeat(257))?;
        assert!(projection.raise(&receipt, oversized).is_err());
        Ok(())
    }

    #[test]
    fn constructors_validate_names_and_debug_redacts_definition_content()
    -> Result<(), Box<dyn std::error::Error>> {
        let registry = registry()?;
        let identity = registry
            .identity(&ToolName::new("read")?)
            .ok_or("registered identity is missing")?
            .clone();
        let invalid = ToolExposure::new(identity.clone(), "contains a space");
        assert_eq!(
            invalid.err().map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::Registration)
        );

        let map = ToolExposureMap::new(registry, [ToolExposure::new(identity, "read_file")?])?;
        assert_eq!(map.registry_digest().domain(), DigestDomain::ToolDefinition);
        assert_eq!(map.registry_digest().version(), 9);
        assert_eq!(map.digest().domain(), DigestDomain::RequestPlan);
        assert_eq!(map.digest().version(), 8);
        assert_eq!(
            map.registry_digest().to_string(),
            "3a1a7da6f5e0d58c5120c4cb8372517503293a40f29c781dd1dbbe435d9acb2b"
        );
        let debug = format!("{map:?}");
        assert!(!debug.contains("SENSITIVE_DESCRIPTION_SENTINEL"));
        assert!(!debug.contains("SENSITIVE_SCHEMA_SENTINEL"));
        assert!(!debug.contains("properties"));
        Ok(())
    }

    #[test]
    fn description_override_is_model_facing_and_digest_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let registry = registry()?;
        let identity = registry
            .identity(&ToolName::new("read")?)
            .ok_or("registered identity is missing")?
            .clone();
        let canonical_digest = registry
            .definition(&ToolName::new("read")?)
            .ok_or("registered definition is missing")?
            .digest();
        let plain = ToolExposureMap::new(
            registry.clone(),
            [ToolExposure::new(identity.clone(), "read_file")?],
        )?;
        let overridden = ToolExposureMap::new(
            registry.clone(),
            [ToolExposure::new(identity, "read_file")?
                .with_description_override(Some("provider-facing description".to_owned()))?],
        )?;

        assert_eq!(
            overridden.definitions()[0].description(),
            "provider-facing description"
        );
        assert_eq!(
            registry
                .definition(&ToolName::new("read")?)
                .ok_or("registered definition is missing")?
                .digest(),
            canonical_digest
        );
        assert_ne!(plain.digest(), overridden.digest());
        assert_eq!(
            overridden.entries()[0].description_override(),
            Some("provider-facing description")
        );
        Ok(())
    }

    #[test]
    fn codex_absolute_workdir_is_canonicalized_only_beneath_the_active_cwd()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        std::fs::create_dir(workspace.path().join("nested"))?;
        let environment = ExecutionEnvironment::host(
            crate::ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000091",
            )?),
            workspace.path(),
            ".",
            crate::ExecutionEnvironmentConfig::default(),
        )?;

        assert_eq!(
            canonical_process_workdir(&environment, environment.cwd())?,
            Path::new(".")
        );
        assert_eq!(
            canonical_process_workdir(&environment, &environment.cwd().join("nested"))?,
            Path::new("nested")
        );
        assert_eq!(
            canonical_process_workdir(&environment, Path::new("nested"))?,
            Path::new("nested")
        );
        assert!(canonical_process_workdir(&environment, Path::new("../outside")).is_err());
        assert!(canonical_process_workdir(&environment, Path::new("/tmp/outside")).is_err());
        Ok(())
    }
}
