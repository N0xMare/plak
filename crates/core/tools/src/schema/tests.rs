use super::*;
use plakous_model::{
    PortableToolSchemaV2, ToolConcurrency, ToolEffect, ToolInputContract, ToolName,
    ToolOutputContract,
};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn definition() -> TestResult<ToolDefinition> {
    definition_with_schema(json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object",
        "properties":{"value":{"type":"integer"}},
        "required":["value"],
        "additionalProperties":false
    }))
}

fn definition_with_schema(schema: Value) -> TestResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new("lookup")?,
        "1",
        "lookup",
        ToolInputContract::Structured(PortableToolSchemaV2::new(schema)?),
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Parallel,
        ToolEffect::ReadOnly,
    )?)
}

#[test]
fn numeric_rejection_names_the_limit_without_echoing_other_arguments() -> TestResult {
    let definition = definition_with_schema(json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object", "properties":{
            "timeout":{"type":"integer","minimum":0,"maximum":300_000},
            "command":{"type":"string"}
        }, "required":["command"], "additionalProperties":false
    }))?;
    let schema = ValidatedToolSchema::compile(
        &definition,
        SchemaLimits::default(),
        ArgumentLimits::default(),
    )?;
    let error = schema
        .validate_arguments(r#"{"command":"private-command-sentinel","timeout":600000}"#)
        .err()
        .ok_or("an excessive timeout must fail validation")?;
    assert_eq!(error.kind(), ToolRuntimeErrorKind::InvalidArguments);
    assert!(
        error
            .message()
            .as_str()
            .contains("timeout must be at most 300000")
    );
    assert!(
        !error
            .message()
            .as_str()
            .contains("private-command-sentinel")
    );
    assert!(
        schema
            .validate_arguments(r#"{"command":"ok","timeout":300000}"#)
            .is_ok()
    );
    Ok(())
}

#[test]
fn local_validation_accepts_an_exact_portable_instance() -> TestResult {
    let schema = ValidatedToolSchema::compile(
        &definition()?,
        SchemaLimits::default(),
        ArgumentLimits::default(),
    )?;
    assert!(schema.validate_arguments(r#"{"value":2}"#).is_ok());
    assert!(
        schema
            .validate_arguments(r#"{"value":2,"value":3}"#)
            .is_err()
    );
    assert!(
        schema
            .validate_arguments(r#"{"value":2,"optional":null}"#)
            .is_err()
    );
    assert!(
        schema
            .validate_arguments(r#"{"value":2,"optional":"x"}"#)
            .is_err()
    );
    Ok(())
}

#[test]
fn compile_rejects_zero_or_wider_argument_limits() -> TestResult {
    let defaults = ArgumentLimits::default();
    let invalid_limits = [
        ArgumentLimits {
            max_bytes: 0,
            ..defaults
        },
        ArgumentLimits {
            max_depth: 0,
            ..defaults
        },
        ArgumentLimits {
            max_nodes: 0,
            ..defaults
        },
        ArgumentLimits {
            max_string_bytes: 0,
            ..defaults
        },
        ArgumentLimits {
            max_bytes: defaults.max_bytes + 1,
            ..defaults
        },
        ArgumentLimits {
            max_depth: defaults.max_depth + 1,
            ..defaults
        },
        ArgumentLimits {
            max_nodes: defaults.max_nodes + 1,
            ..defaults
        },
        ArgumentLimits {
            max_string_bytes: defaults.max_string_bytes + 1,
            ..defaults
        },
    ];
    for limits in invalid_limits {
        let error = ValidatedToolSchema::compile(&definition()?, SchemaLimits::default(), limits)
            .err()
            .ok_or("invalid argument limits compiled")?;
        assert_eq!(error.kind(), ToolRuntimeErrorKind::InvalidDefinition);
    }
    Ok(())
}

#[test]
fn structured_byte_limit_accepts_exactly_at_limit_and_rejects_plus_one() -> TestResult {
    let schema = ValidatedToolSchema::compile(
        &definition_with_schema(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{},
            "required":[],
            "additionalProperties":false
        }))?,
        SchemaLimits::default(),
        ArgumentLimits {
            max_bytes: 2,
            ..ArgumentLimits::default()
        },
    )?;
    assert!(schema.validate_arguments("{}").is_ok());
    assert_eq!(
        schema
            .validate_arguments(" {}")
            .err()
            .map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::ResourceLimit)
    );
    Ok(())
}

#[test]
fn structured_depth_limit_accepts_exactly_at_limit_and_rejects_plus_one() -> TestResult {
    let limits = ArgumentLimits {
        max_depth: 2,
        ..ArgumentLimits::default()
    };
    let at_limit = ValidatedToolSchema::compile(
        &definition_with_schema(array_definition(1))?,
        SchemaLimits::default(),
        limits,
    )?;
    let plus_one = ValidatedToolSchema::compile(
        &definition_with_schema(array_definition(2))?,
        SchemaLimits::default(),
        limits,
    )?;
    assert!(at_limit.validate_arguments(r#"{"value":[1]}"#).is_ok());
    assert_eq!(
        plus_one
            .validate_arguments(r#"{"value":[[1]]}"#)
            .err()
            .map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::ResourceLimit)
    );
    Ok(())
}

#[test]
fn structured_node_limit_accepts_exactly_at_limit_and_rejects_plus_one() -> TestResult {
    let schema = ValidatedToolSchema::compile(
        &definition_with_schema(array_definition(1))?,
        SchemaLimits::default(),
        ArgumentLimits {
            max_nodes: 4,
            ..ArgumentLimits::default()
        },
    )?;
    assert!(schema.validate_arguments(r#"{"value":[1,2]}"#).is_ok());
    assert_eq!(
        schema
            .validate_arguments(r#"{"value":[1,2,3]}"#)
            .err()
            .map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::ResourceLimit)
    );
    Ok(())
}

#[test]
fn structured_string_limit_accepts_exactly_at_limit_and_rejects_plus_one() -> TestResult {
    let limits = ArgumentLimits {
        max_string_bytes: 4,
        ..ArgumentLimits::default()
    };
    let value_schema = ValidatedToolSchema::compile(
        &definition_with_schema(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{"v":{"type":"string"}},
            "required":["v"],
            "additionalProperties":false
        }))?,
        SchemaLimits::default(),
        limits,
    )?;
    assert!(value_schema.validate_arguments(r#"{"v":"1234"}"#).is_ok());
    assert_eq!(
        value_schema
            .validate_arguments(r#"{"v":"12345"}"#)
            .err()
            .map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::ResourceLimit)
    );

    let at_limit_key = ValidatedToolSchema::compile(
        &definition_with_schema(key_definition("1234"))?,
        SchemaLimits::default(),
        limits,
    )?;
    let plus_one_key = ValidatedToolSchema::compile(
        &definition_with_schema(key_definition("12345"))?,
        SchemaLimits::default(),
        limits,
    )?;
    assert!(at_limit_key.validate_arguments(r#"{"1234":1}"#).is_ok());
    assert_eq!(
        plus_one_key
            .validate_arguments(r#"{"12345":1}"#)
            .err()
            .map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::ResourceLimit)
    );
    Ok(())
}

fn array_definition(nested_arrays: usize) -> Value {
    let mut item = json!({"type":"integer"});
    for _ in 0..nested_arrays {
        item = json!({"type":"array", "items":item});
    }
    json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object",
        "properties":{"value":item},
        "required":["value"],
        "additionalProperties":false
    })
}

fn key_definition(key: &str) -> Value {
    json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object",
        "properties":{key:{"type":"integer"}},
        "required":[key],
        "additionalProperties":false
    })
}
