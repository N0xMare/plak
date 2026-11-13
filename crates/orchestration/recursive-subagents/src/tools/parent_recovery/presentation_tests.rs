use super::*;

#[test]
fn recovery_remedy_uses_the_presented_input_schema() -> Result<(), Box<dyn std::error::Error>> {
    for acknowledgement in [false, true] {
        let mut properties = json!({"agent_id":{"type":"string"}});
        if acknowledgement {
            properties["acknowledge_recovery"] = json!({"type":"string"});
        }
        let definition = ToolDefinition::new(
            ToolName::new("send_agent_message")?,
            "1.0.0",
            "send",
            ToolInputContract::structured(
                json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":properties,"required":["agent_id"],"additionalProperties":false}),
            )?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::Mutating,
        )?;
        let outcome = ToolOutcome::failure_text(
            json!({"code":"followup_recovery_required",
            "message":"recipient requires recovery; send acknowledge_recovery=attempt"})
            .to_string(),
        )?;
        let result = present_refusal(outcome, &definition)?.joined_text();
        assert_eq!(result.contains("acknowledge_recovery"), acknowledgement);
        assert_eq!(result.contains("Resume/recover"), !acknowledgement);
    }
    Ok(())
}
