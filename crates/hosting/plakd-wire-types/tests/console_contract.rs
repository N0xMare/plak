//! Protocol-six console examples are independent of local product preferences.

use plakd_wire_types::console::{HostRequest, HostResponse};
use serde_json::json;

#[test]
fn semantic_validation_issues_require_a_field_code_and_safe_detail()
-> Result<(), Box<dyn std::error::Error>> {
    for (field, code, detail) in [
        (
            "body.working_set",
            "empty",
            "working_set must contain at least one template",
        ),
        (
            "body.root",
            "unknown_template",
            "the root template does not exist",
        ),
        (
            "roster.working_set",
            "type_mismatch",
            "the value has the wrong kind for this setting",
        ),
    ] {
        let example = json!({"response":"refused","reason":{"reason":"validation_failed",
            "issues":[{"field":field,"code":code,"detail":detail}]}});
        let response: HostResponse = serde_json::from_value(example.clone())?;
        assert_eq!(serde_json::to_value(response)?, example);
        let mut missing = example;
        missing["reason"]["issues"][0]
            .as_object_mut()
            .ok_or("issue object")?
            .remove("detail");
        assert!(serde_json::from_value::<HostResponse>(missing).is_err());
    }
    Ok(())
}

#[test]
fn every_console_request_has_an_exact_attachment_free_body()
-> Result<(), Box<dyn std::error::Error>> {
    for example in [
        json!({"request":"accounts_status","account_id":"codex/work","limit":25}),
        json!({"request":"accounts_usage","window":"24h"}),
        json!({"request":"profiles_list","limit":10}),
        json!({"request":"profiles_get","id":"codex-default"}),
        json!({"request":"templates_list","cursor":"opaque","limit":10}),
        json!({"request":"templates_get","id":"worker"}),
        json!({"request":"config_get"}),
        json!({"request":"console_metadata_get","account_id":"codex/work"}),
    ] {
        let decoded: HostRequest = serde_json::from_value(example.clone())?;
        assert_eq!(serde_json::to_value(decoded)?, example);
        let mut with_session = example;
        with_session["session_id"] = json!("not-an-admission-selector");
        assert!(serde_json::from_value::<HostRequest>(with_session).is_err());
    }
    Ok(())
}

#[test]
fn definition_outputs_require_nullable_facts_and_keep_withheld_values_structural()
-> Result<(), Box<dyn std::error::Error>> {
    let example = json!({"response":"profiles_list","etag":"opaque","current":null,
        "rows":[],"diagnostics":[],"page":{"next":null,"truncated":false}});
    let decoded: HostResponse = serde_json::from_value(example.clone())?;
    assert_eq!(serde_json::to_value(decoded)?, example);
    let mut missing = example;
    missing
        .as_object_mut()
        .ok_or("response object")?
        .remove("current");
    assert!(serde_json::from_value::<HostResponse>(missing).is_err());
    let schema = serde_json::to_value(schemars::schema_for!(HostResponse))?;
    let variants = schema["oneOf"].as_array().ok_or("response variants")?;
    let profile = variants
        .iter()
        .find(|variant| variant["properties"]["response"]["const"] == "profiles_list")
        .ok_or("profiles schema")?;
    assert!(
        profile["required"]
            .as_array()
            .ok_or("required properties")?
            .contains(&json!("current"))
    );
    assert_eq!(
        profile["properties"]["current"]["type"],
        json!(["string", "null"])
    );
    let withheld = json!({"state":"withheld"});
    let decoded: plakd_wire_types::console::ProtectedText =
        serde_json::from_value(withheld.clone())?;
    assert_eq!(serde_json::to_value(decoded)?, withheld);
    assert!(
        serde_json::from_value::<plakd_wire_types::console::ProtectedText>(
            json!({"state":"withheld","literal":"not allowed"})
        )
        .is_err()
    );
    Ok(())
}
