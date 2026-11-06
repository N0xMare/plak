#[test]
fn hosted_tool_call_final_dial_is_admitted() {
    let document = serde_json::json!({"version": 1, "hosted_tool_call_final": true});
    let plan: ScriptPlan = serde_json::from_value(document).expect("hosted final dial");
    plan.validate().expect("valid hosted final");
    assert_eq!(
        serde_json::to_value(plan).unwrap()["hosted_tool_call_final"],
        true
    );
}

use super::*;

#[test]
fn a_script_reads_its_closed_document_shape() {
    let plan: ScriptPlan = serde_json::from_str(
        r#"{"version":1,"steps":[
                {"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls"},
                {"output":{"text":"done"},"usage":{"output":7}}
            ]}"#,
    )
    .expect("closed document");
    plan.validate().expect("admitted");
    assert_eq!(plan.maximum_projected_input, 10_000_000);
    assert!(matches!(
        plan.step(1).map(|step| &step.output),
        Some(ScriptOutput::Tool(call)) if call.name == "loopback_effect"
    ));
    assert_eq!(
        plan.step(2).and_then(|step| step.usage).map(|u| u.output),
        Some(7)
    );
    assert!(plan.step(3).is_none());
}

#[test]
fn a_round_may_request_several_tools_but_not_none_or_an_empty_name() {
    let plan: ScriptPlan = serde_json::from_str(
        r#"{"version":1,"steps":[
                {"output":{"tools":[{"name":"edit"},{"name":"write"}]},"finish":"tool_calls"}
            ]}"#,
    )
    .expect("closed document");
    plan.validate().expect("admitted");
    assert!(matches!(
        plan.step(1).map(|step| &step.output),
        Some(ScriptOutput::Tools(calls))
            if calls.iter().map(|call| call.name.as_str()).eq(["edit", "write"])
    ));
    for refused in [
        r#"{"version":1,"steps":[{"output":{"tools":[]}}]}"#,
        r#"{"version":1,"steps":[{"output":{"tools":[{"name":"edit"},{"name":" "}]}}]}"#,
    ] {
        let plan: ScriptPlan = serde_json::from_str(refused).expect("document");
        assert!(plan.validate().is_err(), "{refused}");
    }
}

#[test]
fn a_foreign_version_is_refused() {
    let plan: ScriptPlan = serde_json::from_str(r#"{"version":9,"steps":[]}"#).expect("document");
    assert!(plan.validate().is_err());
}

#[test]
fn an_unknown_field_is_refused() {
    assert!(serde_json::from_str::<ScriptPlan>(r#"{"version":1,"nope":true}"#).is_err());
}

#[test]
fn sealed_family_artifacts_require_a_supported_registry() {
    let admitted: ScriptPlan =
        serde_json::from_str(r#"{"version":1,"seal_family_artifacts":true,"steps":[]}"#)
            .expect("document");
    admitted.validate().expect("artifact loopback");
    assert!(admitted.seal_family_artifacts);
    for registry in ["tree", "code_mode"] {
        let mut document = serde_json::to_value(&admitted).expect("document");
        document[registry] = serde_json::json!(true);
        let refused: ScriptPlan = serde_json::from_value(document).expect("document");
        assert!(refused.validate().is_err());
    }
    let mut process = admitted;
    process.process_tools = true;
    process.validate().expect("artifact process registry");
}

#[test]
fn a_programmatic_call_requires_sealed_family_artifacts() {
    let document = serde_json::json!({"version": 1, "steps": [
        {"output": {"tool": {"name": "loopback_effect", "programmatic": true}}, "finish": "tool_calls"}
    ]});
    let refused: ScriptPlan = serde_json::from_value(document.clone()).expect("document");
    assert!(refused.validate().is_err());
    let mut sealed = document;
    sealed["seal_family_artifacts"] = serde_json::json!(true);
    let admitted: ScriptPlan = serde_json::from_value(sealed).expect("document");
    admitted.validate().expect("a sealed programmatic call");
}

#[test]
fn an_identity_namespace_must_fit_the_high_half_of_a_call_id() {
    let admitted: ScriptPlan =
        serde_json::from_str(r#"{"version":1,"identity_namespace":2,"steps":[]}"#)
            .expect("document");
    admitted.validate().expect("48-bit namespace");
    let refused: ScriptPlan =
        serde_json::from_str(r#"{"version":1,"identity_namespace":281474976710656,"steps":[]}"#)
            .expect("document");
    assert!(refused.validate().is_err());
}

#[test]
fn tree_durability_dials_require_a_tree_and_durable_locator_publishes() {
    let bare: ScriptPlan =
        serde_json::from_str(r#"{"version":1,"child_hold_ms":1,"steps":[]}"#).expect("document");
    assert!(bare.validate().unwrap_err().message().contains("tree"));
    let memory: ScriptPlan = serde_json::from_str(
        r#"{"version":1,"tree":true,"disk_full_at_child_locator":true,"steps":[]}"#,
    )
    .expect("document");
    assert!(
        memory
            .validate()
            .unwrap_err()
            .message()
            .contains("durable_tree")
    );
    let admitted: ScriptPlan = serde_json::from_str(
        r#"{"version":1,"tree":true,"durable_tree":true,"disk_full_at_child_locator":true,"child_hold_ms":1000,"child_hold_after":1,"fail_child_after_successes":2,"steps":[]}"#,
    )
    .expect("document");
    admitted.validate().expect("admitted");
    assert!(admitted.disk_full_at_child_locator);
    assert_eq!(admitted.fail_child_after_successes, Some(2));
}

#[test]
fn a_repeating_script_replays_its_last_step() {
    let plan: ScriptPlan = serde_json::from_str(
        r#"{"version":1,"repeat_last_step":true,"steps":[{"output":{"text":"a"}}]}"#,
    )
    .expect("document");
    assert!(matches!(
        plan.step(4).map(|step| &step.output),
        Some(ScriptOutput::Text(text)) if text == "a"
    ));
}
