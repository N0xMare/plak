//! Product DTO examples pinned before extraction.

#![allow(clippy::unwrap_used, reason = "contract-owned codec values")]

#[test]
fn product_wire_contract_examples_decode_without_local_preferences() {
    use plakd_wire_types::{SurfaceIntent, SurfaceObservation};
    use serde_json::json;
    let product = json!({
        "default_account":null,"default_model":null,"default_effort":null,"default_extra":null,
        "composition":"direct","protocol":"auto","composition_options":[],"compaction":true
    });
    for value in [
        json!("open_settings"),
        json!({"save_settings":{"product":product.clone()}}),
    ] {
        let decoded: SurfaceIntent = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    }
    for value in [
        json!({"settings_view":{"product":product}}),
        json!({"workspace_chrome":{"path":"workspace","home":null,"branch":"main"}}),
        json!({"reload_requested":{"version":1,"request_id":"00000000-0000-0000-0000-000000000001","session_id":"00000000-0000-0000-0000-000000000002","expect_binary":"fixture-binary","nonce":"fixture-instance"}}),
        json!({"reload_probed":{"target":{"executable":"plak","capacity":{"route":"fixture","total_window":1000,"admitted_input":900,"estimated_tokens":100,"messages":2},"capacity_error":null,"state_key":"fixture-key","probe":{"state_key":"fixture-key","export_versions":[12,11],"config_error":null,"binary_id":null},"reopen":{"verdict":"unknown","reason":"pending"},"request":null},"refusal":null}}),
    ] {
        let decoded: SurfaceObservation = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    }
}
