use plakous_model::{
    SemanticJsonV1, ToolConcurrency, ToolEffect, ToolInputContract, ToolOutputContract,
};
use serde_json::{Value, json};

use super::*;
use crate::ToolOutcome;

type TestTool = (ToolDefinition, Arc<crate::test_support::FixedTool>);

fn tool(name: &str) -> Result<TestTool, Box<dyn std::error::Error>> {
    tool_with_schema(
        name,
        json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false}),
    )
}

fn tool_with_schema(name: &str, schema: Value) -> Result<TestTool, Box<dyn std::error::Error>> {
    let definition = ToolDefinition::new(
        ToolName::new(name)?,
        "1.0.0",
        "A test tool.",
        ToolInputContract::structured(schema)?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    Ok((
        definition,
        Arc::new(crate::test_support::FixedTool::new(
            ToolOutcome::success_text("ok")?,
        )),
    ))
}

fn registration(
    tool_id: &str,
    executor_id: &str,
    tool: TestTool,
) -> Result<ToolRegistration, Box<dyn std::error::Error>> {
    Ok(ToolRegistration::new(
        RegisteredToolId::new(tool_id)?,
        ToolExecutorId::new(executor_id)?,
        ToolExecutorBindingIdentity::new(1, &[executor_id.as_bytes()])?,
        tool.0,
        tool.1,
    ))
}

fn binding(value: &str) -> Result<ToolExecutorBindingIdentity, ToolRuntimeError> {
    ToolExecutorBindingIdentity::new(1, &[value.as_bytes()])
}

#[test]
fn mcp_server_name_grammar_table() {
    for valid in [
        "a",
        "github",
        "gh2",
        "abcdefghijklmnop",
        "my_server",
        "a_b_c",
        "linear_2",
        "abcdefghijklmnop_qrstuvwxyz01234",
    ] {
        assert!(McpServerName::new(valid).is_ok(), "{valid}");
    }
    for invalid in [
        "",
        "Github",
        "2fa",
        "_github",
        "github_",
        "git__hub",
        "git-hub",
        "git.hub",
        "abcdefghijklmnopq",
        "abcdefghijklmnop_qrstuvwxyz012345",
        "gith\u{fc}b",
    ] {
        assert_eq!(
            McpServerName::new(invalid).err().map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::Registration),
            "{invalid}"
        );
    }
}

#[test]
fn owner_class_defaults_local_stamps_results_and_moves_no_identity()
-> Result<(), Box<dyn std::error::Error>> {
    use std::str::FromStr;

    let github = ToolOwnerClass::Mcp(McpServerName::new("github")?);
    assert_eq!(
        github.execution_owner(),
        ToolExecutionOwner::Mcp {
            server: McpServerId::new("github")?
        }
    );
    assert_eq!(
        ToolOwnerClass::default().execution_owner(),
        ToolExecutionOwner::Local
    );
    let freeze =
        |owner: Option<ToolOwnerClass>| -> Result<FrozenToolRegistry, Box<dyn std::error::Error>> {
            let registration = registration("lookup-id", "lookup-exec", tool("lookup")?)?;
            assert_eq!(registration.owner(), &ToolOwnerClass::PlakLocal);
            let mut builder = ToolRegistryBuilder::default();
            builder.register(match owner {
                Some(owner) => registration.with_owner(owner),
                None => registration,
            })?;
            Ok(builder.freeze())
        };
    let local = freeze(None)?;
    let mcp = freeze(Some(github.clone()))?;
    assert_eq!(
        local.registry_identity_digest()?,
        mcp.registry_identity_digest()?
    );
    let name = ToolName::new("lookup")?;
    assert_eq!(
        mcp.identity(&name).map(RegisteredToolIdentity::owner),
        Some(&github)
    );

    let result =
        |registry: &FrozenToolRegistry| -> Result<ToolExecutionOwner, Box<dyn std::error::Error>> {
            let invocation = registry.prepare(ToolInvocationRequest::new(
                plakous_model::RunId::from_str("00000000-0000-0000-0000-000000000001")?,
                plakous_model::TurnId::from_str("00000000-0000-0000-0000-000000000002")?,
                plakous_model::AttemptId::from_str("00000000-0000-0000-0000-000000000003")?,
                plakous_model::CallId::from_str("00000000-0000-0000-0000-000000000004")?,
                name.clone(),
                plakous_model::ToolCorrelation::default(),
                "{}",
            ))?;
            Ok(ToolOutcome::success_text("ok")?
                .to_model_result(&invocation)?
                .owner()
                .clone())
        };
    assert_eq!(result(&local)?, ToolExecutionOwner::Local);
    assert_eq!(result(&mcp)?, github.execution_owner());
    Ok(())
}

#[test]
fn duplicate_name_fails_in_isolation() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = ToolRegistryBuilder::default();
    builder.register(registration("lookup-id", "lookup-exec", tool("lookup")?)?)?;
    let duplicate = builder.register(registration("other-id", "other-exec", tool("lookup")?)?);
    assert_eq!(
        duplicate.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Registration)
    );

    assert_eq!(builder.freeze().len(), 1);
    Ok(())
}

#[test]
fn nested_eligibility_defaults_closed_and_rotates_registry_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let mut direct = ToolRegistryBuilder::default();
    direct.register(registration("lookup-id", "lookup-exec", tool("lookup")?)?)?;
    let direct = direct.freeze();
    let name = ToolName::new("lookup")?;
    assert_eq!(
        direct.nested_eligibility(&name),
        Some(NestedToolEligibility::DirectOnly)
    );

    let mut nested = ToolRegistryBuilder::default();
    nested
        .register(registration("lookup-id", "lookup-exec", tool("lookup")?)?.nested_eligible())?;
    let nested = nested.freeze();
    assert_eq!(
        nested.nested_eligibility(&name),
        Some(NestedToolEligibility::NestedEligible)
    );
    assert_ne!(direct.identity(&name), nested.identity(&name));
    assert_ne!(
        direct.registry_identity_digest()?,
        nested.registry_identity_digest()?
    );
    Ok(())
}

#[test]
fn duplicate_tool_id_fails_in_isolation() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = ToolRegistryBuilder::default();
    builder.register(registration("stable-id", "first-exec", tool("first")?)?)?;
    let duplicate = builder.register(registration("stable-id", "second-exec", tool("second")?)?);
    assert_eq!(
        duplicate.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Registration)
    );
    assert_eq!(builder.freeze().len(), 1);
    Ok(())
}

#[test]
fn executor_id_alias_requires_the_exact_shared_binding() -> Result<(), Box<dyn std::error::Error>> {
    let (first, executor) = tool("first")?;
    let (second, different_executor) = tool("second")?;
    let mut rejected = ToolRegistryBuilder::default();
    rejected.register(ToolRegistration::new(
        RegisteredToolId::new("first-id")?,
        ToolExecutorId::new("shared-exec")?,
        binding("shared-exec")?,
        first.clone(),
        executor.clone(),
    ))?;
    let alias = rejected.register(ToolRegistration::new(
        RegisteredToolId::new("second-id")?,
        ToolExecutorId::new("shared-exec")?,
        binding("shared-exec")?,
        second.clone(),
        different_executor,
    ));
    assert_eq!(
        alias.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Registration)
    );
    assert_eq!(rejected.freeze().len(), 1);

    let shared: Arc<dyn ToolExecutor> = executor;
    let mut accepted = ToolRegistryBuilder::default();
    accepted.register(ToolRegistration::new(
        RegisteredToolId::new("first-id")?,
        ToolExecutorId::new("shared-exec")?,
        binding("shared-exec")?,
        first,
        Arc::clone(&shared),
    ))?;
    accepted.register(ToolRegistration::new(
        RegisteredToolId::new("second-id")?,
        ToolExecutorId::new("shared-exec")?,
        binding("shared-exec")?,
        second,
        shared,
    ))?;
    assert_eq!(accepted.freeze().len(), 2);
    Ok(())
}

#[test]
fn reserved_name_fails_in_isolation() -> Result<(), Box<dyn std::error::Error>> {
    let mut reserved = ToolRegistryBuilder::default();
    assert!(
        reserved
            .register(registration(
                "reserved-id",
                "reserved-exec",
                tool("plak.internal")?,
            )?)
            .is_err()
    );
    Ok(())
}

#[test]
fn max_tools_overflow_is_typed_and_error_atomic() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits {
        max_tools: 1,
        ..ToolRegistryLimits::default()
    });
    builder.register(registration("first-id", "first-exec", tool("first")?)?)?;

    let error = builder
        .register(registration(
            "rejected-id",
            "rejected-exec",
            tool("rejected")?,
        )?)
        .err()
        .ok_or("tool-count overflow was accepted")?;
    assert_eq!(error.kind(), ToolRuntimeErrorKind::ResourceLimit);
    assert_eq!(
        error.to_string(),
        "tool registry exceeds its tool-count limit"
    );

    let frozen = builder.freeze();
    assert_eq!(frozen.len(), 1);
    assert!(frozen.identity(&ToolName::new("first")?).is_some());
    assert!(frozen.identity(&ToolName::new("rejected")?).is_none());
    Ok(())
}

#[test]
fn max_total_schema_bytes_overflow_is_typed_and_error_atomic()
-> Result<(), Box<dyn std::error::Error>> {
    let small_schema = json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false});
    let rejected_schema = json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object",
        "properties":{"payload":{"type":"string"}},
        "required":["payload"],
        "additionalProperties":false
    });
    let small_bytes = SemanticJsonV1::from_value(small_schema.clone())?
        .canonical_bytes()
        .len();
    assert!(
        SemanticJsonV1::from_value(rejected_schema.clone())?
            .canonical_bytes()
            .len()
            > small_bytes
    );
    let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits {
        max_tools: 3,
        max_total_schema_bytes: small_bytes.saturating_mul(2),
        ..ToolRegistryLimits::default()
    });
    builder.register(registration(
        "first-id",
        "first-exec",
        tool_with_schema("first", small_schema.clone())?,
    )?)?;

    let error = builder
        .register(registration(
            "rejected-id",
            "rejected-exec",
            tool_with_schema("rejected", rejected_schema)?,
        )?)
        .err()
        .ok_or("total schema-byte overflow was accepted")?;
    assert_eq!(error.kind(), ToolRuntimeErrorKind::ResourceLimit);
    assert_eq!(
        error.to_string(),
        "tool registry exceeds its total schema-byte limit"
    );

    builder.register(registration(
        "third-id",
        "third-exec",
        tool_with_schema("third", small_schema)?,
    )?)?;
    let frozen = builder.freeze();
    assert_eq!(frozen.len(), 2);
    assert_eq!(
        frozen
            .definitions()
            .iter()
            .map(|definition| definition.name().as_str())
            .collect::<Vec<_>>(),
        ["first", "third"]
    );
    assert!(frozen.identity(&ToolName::new("rejected")?).is_none());
    Ok(())
}

#[test]
fn executor_has_no_descriptor_authority_and_revision_drift_fails_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let (first, executor) = tool("first")?;
    let mut drifted = first.clone();
    drifted = ToolDefinition::new(
        drifted.name().clone(),
        "2.0.0",
        drifted.description(),
        drifted.input().clone(),
        drifted.output().clone(),
        drifted.concurrency().clone(),
        drifted.effect(),
    )?;
    let mut builder = ToolRegistryBuilder::default();
    builder.register(ToolRegistration::new(
        RegisteredToolId::new("stable")?,
        ToolExecutorId::new("executor")?,
        binding("executor")?,
        first.clone(),
        executor.clone(),
    ))?;
    let drift = builder.register(ToolRegistration::new(
        RegisteredToolId::new("stable")?,
        ToolExecutorId::new("executor")?,
        binding("executor")?,
        drifted,
        executor,
    ));
    assert_eq!(
        drift.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Registration)
    );
    let frozen = builder.freeze();
    assert_eq!(frozen.definitions()[0].name().as_str(), "first");
    assert_eq!(frozen.definitions()[0], first);
    Ok(())
}
