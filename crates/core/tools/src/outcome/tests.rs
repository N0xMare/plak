use super::*;

#[test]
fn typed_value_moves_without_changing_or_exposing_canonical_content() -> Result<(), ToolRuntimeError>
{
    let value = crate::ExecutionValue::Process(crate::ProcessValue {
        job_id: 1,
        omitted_bytes: 0,
        status: "exited".into(),
        exit_code: Some(0),
        session_id: None,
        output: "private typed content".into(),
        truncated: false,
        full_output: None,
    });
    let plain = ToolOutcome::success_text("same text")?;
    let mut typed = ToolOutcome::success_text("same text")?.with_value(value.clone())?;
    assert_eq!(plain.parts(), typed.parts());
    assert_eq!(plain.retained_bytes(), typed.retained_bytes());
    assert!(!format!("{typed:?}").contains("private typed content"));
    assert_eq!(typed.take_value(), Some(value));
    assert!(typed.take_value().is_none());
    Ok(())
}

#[test]
fn typed_payloads_obey_default_and_reduced_runtime_memory_bounds() -> Result<(), ToolRuntimeError> {
    let value = |bytes| {
        crate::ExecutionValue::Process(crate::ProcessValue {
            job_id: 1,
            omitted_bytes: 0,
            status: "exited".into(),
            exit_code: Some(0),
            session_id: None,
            output: "x".repeat(bytes),
            truncated: false,
            full_output: None,
        })
    };
    assert!(
        ToolOutcome::success_text("small text")?
            .with_value(value(MAX_TOOL_OUTCOME_BYTES))
            .is_err()
    );
    let outcome = ToolOutcome::success_text("small text")?.with_value(value(1024))?;
    assert!(
        outcome
            .enforce_limits(ToolOutcomeLimits {
                max_parts: 1,
                max_bytes: 512,
            })
            .is_err()
    );
    Ok(())
}

fn structured_output() -> Result<ToolOutputContract, Box<dyn std::error::Error>> {
    Ok(ToolOutputContract::structured(serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"],
        "additionalProperties": false
    }))?)
}

#[test]
fn outcome_bounds_are_enforced_without_flattening_order() -> Result<(), ToolRuntimeError> {
    let parts = vec![
        ToolResultPart::Text(TextBlock::new("first", None).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "test text failed",
                &error,
            )
        })?),
        ToolResultPart::Text(TextBlock::new("second", None).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "test text failed",
                &error,
            )
        })?),
    ];
    let outcome = ToolOutcome::success(parts.clone())?;
    assert_eq!(outcome.parts(), parts);
    assert!(outcome.is_success());
    assert_eq!(outcome.joined_text(), "firstsecond");
    assert!(
        ToolOutcome::new(
            ToolOutcomeKind::Success,
            parts,
            ToolOutcomeLimits {
                max_parts: 1,
                max_bytes: 1024
            }
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn successful_output_contracts_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
    ToolOutcome::success_text(r#"{"answer":"yes"}"#)?.validate_contract(&structured_output()?)?;
    assert!(
        ToolOutcome::success_text(r#"{"answer":1}"#)?
            .validate_contract(&structured_output()?)
            .is_err()
    );

    let utf8 = ToolOutputContract::utf8(4)?;
    ToolOutcome::success_text("four")?.validate_contract(&utf8)?;
    assert!(
        ToolOutcome::success_text("five!")?
            .validate_contract(&utf8)
            .is_err()
    );

    // Failure content reports the failure and is not reclassified as a
    // successful semantic output.
    ToolOutcome::failure_text("bounded failure")?.validate_contract(&structured_output()?)?;
    Ok(())
}

#[test]
fn output_contract_failure_preserves_process_lifecycle_truth()
-> Result<(), Box<dyn std::error::Error>> {
    use std::num::NonZeroU64;

    for update in [
        crate::internal::ProcessSessionUpdate::Active {
            session_id: [1; 16],
            owner_generation: NonZeroU64::new(1).ok_or("generation")?,
        },
        crate::internal::ProcessSessionUpdate::Joined {
            session_id: [2; 16],
            owner_generation: NonZeroU64::new(2).ok_or("generation")?,
        },
    ] {
        for (effect, expected) in [
            (ToolEffect::ReadOnly, ToolOutcomeKind::Failure),
            (ToolEffect::Mutating, ToolOutcomeKind::OutcomeUnknown),
        ] {
            let outcome = ToolOutcome::success_text("too long")?
                .with_process_session_update(update.clone())
                .with_presentation_subject("retained subject")
                .enforce_completed_output_contract(&ToolOutputContract::utf8(1)?, effect);
            assert_eq!(outcome.kind(), expected);
            assert_eq!(outcome.process_session_update(), Some(update.clone()));
            assert_eq!(outcome.presentation_subject(), Some("retained subject"));
        }
    }
    Ok(())
}

#[test]
fn model_content_contract_enforces_exact_part_and_encoded_byte_limits()
-> Result<(), Box<dyn std::error::Error>> {
    let one = ToolOutcome::success_text("four")?;
    let encoded_bytes = u32::try_from(one.retained_bytes())?;
    assert_eq!(
        one.clone()
            .enforce_completed_output_contract(
                &ToolOutputContract::model_content(1, encoded_bytes)?,
                ToolEffect::ReadOnly
            )
            .kind(),
        ToolOutcomeKind::Success
    );
    assert_eq!(
        one.clone()
            .enforce_completed_output_contract(
                &ToolOutputContract::model_content(1, encoded_bytes - 1)?,
                ToolEffect::ReadOnly
            )
            .kind(),
        ToolOutcomeKind::Failure
    );

    let two = ToolOutcome::success(vec![
        ToolResultPart::Text(TextBlock::new("a", None)?),
        ToolResultPart::Text(TextBlock::new("b", None)?),
    ])?;
    assert_eq!(
        two.enforce_completed_output_contract(
            &ToolOutputContract::model_content(
                1,
                plakous_model::ModelContentContractV1::MAX_BYTES,
            )?,
            ToolEffect::ReadOnly
        )
        .kind(),
        ToolOutcomeKind::Failure
    );
    Ok(())
}
