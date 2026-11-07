//! Default-account binding bytes pinned before named-account support.

use super::*;

pub(super) fn assert_main_bytes(
    provider: PlatformProvider,
    model: &str,
    effort: ReasoningEffort,
    binding: &plakous::sessions::SessionTargetBinding,
) {
    let golden: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/default-api-bindings.json"
    ))
    .unwrap();
    let key = format!("{}/{model}/{effort:?}", provider.account());
    assert_eq!(
        serde_json::json!([
            binding.account_binding().to_string(),
            binding
                .selected_provider_configuration()
                .digest()
                .to_string(),
            binding
                .effective_provider_configuration()
                .digest()
                .to_string(),
        ]),
        golden[&key],
        "default account binding changed: {key}"
    );
}
