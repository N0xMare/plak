use super::{
    CompositionSelection, OpenRouterRoutingSelection, ProtocolSelection, ResolvedProtocol,
    TargetSelection, WIRE_RESOLUTION_REVISION, WireBinding, account_target, admit_account_model,
};
use plakous::subscriptions::NativeProvider;
use plakous_host_protocol::notice_remedy::AccountTarget;

#[test]
fn a_native_provider_maps_to_its_account_without_the_string_table() {
    assert_eq!(account_target(NativeProvider::Codex), AccountTarget::Codex);
    assert_eq!(account_target(NativeProvider::Xai), AccountTarget::Grok);
    assert_eq!(
        account_target(NativeProvider::Claude),
        AccountTarget::Claude
    );
}

#[test]
fn account_model_admission_reads_the_account_catalog() {
    for (account, model) in [
        ("codex", "gpt-5.6-luna"),
        ("chatgpt", "gpt-5.6-luna"),
        ("grok", "grok-4.5"),
        ("supergrok", "grok-4.5"),
        ("claude", "claude-sonnet-5"),
        ("openrouter", "anything/at-all"),
    ] {
        assert_eq!(
            admit_account_model(account, model),
            Ok(()),
            "{account} {model}"
        );
    }
    // A plausible name is not an admission: only catalog rows are.
    for (account, model) in [
        ("codex", "gpt-3"),
        ("codex", "grok-4.5"),
        ("grok", "gpt-5.6-luna"),
        ("grok", "grok-legacy"),
        ("claude", "claude-instant-1"),
        ("claude", "grok-4.5"),
        ("openai", "grok-4.5"),
        ("xai-api", "claude-sonnet-5"),
    ] {
        let refusal = admit_account_model(account, model).unwrap_err();
        assert!(
            refusal.message().contains(&format!("`{model}`")),
            "{refusal}"
        );
        assert!(
            refusal.message().contains(&format!("account `{account}`")),
            "{refusal}"
        );
        assert!(refusal.message().contains("catalog"), "{refusal}");
        assert!(
            matches!(refusal.advice(), Some(plakous_host_protocol::refusal::RefusalAdvice::UnknownModel { account: original, .. }) if original == account)
        );
    }
    assert_eq!(
        admit_account_model("grok", "gpt-5.6-luna")
            .unwrap_err()
            .message(),
        "model `gpt-5.6-luna` is not in the SuperGrok subscription catalog of account `grok`; inspect the account's available models"
    );
    // An account with no catalog is refused, not guessed at.
    assert!(
        admit_account_model("mystery", "gpt-5.6-luna")
            .unwrap_err()
            .message()
            .contains("account `mystery` requires")
    );
}

#[test]
fn composition_selection_has_one_outer_marker_vocabulary() {
    assert_eq!(CompositionSelection::Direct.outer_marker_names(), None);
    assert_eq!(
        CompositionSelection::FollowCatalog.outer_marker_names(),
        None
    );
    assert_eq!(
        CompositionSelection::NativePtc.outer_marker_names(),
        Some(["exec", "wait"].as_slice())
    );
    assert_eq!(
        CompositionSelection::JsPtc.outer_marker_names(),
        Some(["exec_js", "wait_js"].as_slice())
    );
    assert_eq!(
        CompositionSelection::PyPtc.outer_marker_names(),
        Some(["exec_py", "wait_py"].as_slice())
    );
}

#[test]
fn api_targets_reconstruct_without_crossing_account_or_oauth_authority() {
    use super::{AttachRoute, PlatformProvider};
    for provider in [
        PlatformProvider::OpenAi,
        PlatformProvider::Anthropic,
        PlatformProvider::Xai,
    ] {
        let target = TargetSelection::Platform { provider };
        let encoded = serde_json::to_vec(&target).unwrap();
        let restored: TargetSelection = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(restored, target);
        assert_eq!(
            restored.route(provider.account()),
            Ok(AttachRoute::Platform { provider })
        );
        for other in [
            "codex",
            "grok",
            "claude",
            "openrouter",
            "openai",
            "anthropic",
            "xai-api",
        ] {
            if other != provider.account() {
                assert!(restored.route(other).is_err());
            }
        }
        assert!(TargetSelection::Native.route(provider.account()).is_err());
        assert_eq!(
            restored.api_target().unwrap().protocol(),
            provider.protocol()
        );
    }
}

const COMPOSITIONS: [CompositionSelection; 6] = [
    CompositionSelection::Direct,
    CompositionSelection::FollowCatalog,
    CompositionSelection::NativePtc,
    CompositionSelection::JsPtc,
    CompositionSelection::PyPtc,
    CompositionSelection::PySdk,
];
const PROTOCOLS: [ProtocolSelection; 4] = [
    ProtocolSelection::Auto,
    ProtocolSelection::Chat,
    ProtocolSelection::Responses,
    ProtocolSelection::Messages,
];

#[test]
fn config_labels_round_trip_through_the_binding_vocabulary() {
    for composition in COMPOSITIONS {
        assert_eq!(
            CompositionSelection::parse_config(composition.as_str()),
            Ok(composition)
        );
        let wire = plakous_host_protocol::CompositionSelection::from(composition);
        assert_eq!(wire.as_str(), composition.as_str());
        assert_eq!(CompositionSelection::from(wire), composition);
        assert_eq!(
            serde_json::to_string(&composition).unwrap(),
            serde_json::to_string(&wire).unwrap(),
            "config and protocol serialize the same label"
        );
    }
    assert_eq!(
        CompositionSelection::parse("plakous-default"),
        Ok(CompositionSelection::FollowCatalog),
        "plakous-default is an explicit CLI token as well as the default"
    );
    assert!(
        CompositionSelection::parse("follow-catalog").is_err(),
        "the retired follow-catalog spelling is refused everywhere"
    );
    for protocol in PROTOCOLS {
        assert_eq!(ProtocolSelection::parse(protocol.as_str()), Ok(protocol));
        let wire = plakous_host_protocol::ProtocolSelection::from(protocol);
        assert_eq!(wire.as_str(), protocol.as_str());
        assert_eq!(ProtocolSelection::from(wire), protocol);
    }
}

#[test]
fn wire_binding_resolves_once_and_validates_its_revision() {
    for protocol in PROTOCOLS {
        let binding = WireBinding::resolve(protocol);
        assert_eq!(binding.resolution_revision, WIRE_RESOLUTION_REVISION);
        assert!(binding.validate().is_ok());
        let expected = match protocol {
            ProtocolSelection::Auto | ProtocolSelection::Chat => ResolvedProtocol::Chat,
            ProtocolSelection::Responses => ResolvedProtocol::Responses,
            ProtocolSelection::Messages => ResolvedProtocol::Messages,
        };
        assert_eq!(binding.resolved, expected);
        let decoded: WireBinding =
            serde_json::from_str(&serde_json::to_string(&binding).unwrap()).unwrap();
        assert_eq!(decoded, binding);
    }
    let mut stale = WireBinding::resolve(ProtocolSelection::Auto);
    stale.resolution_revision = "other".into();
    assert!(stale.validate().is_err());
    let mut contradicted = WireBinding::resolve(ProtocolSelection::Responses);
    contradicted.resolved = ResolvedProtocol::Chat;
    assert!(contradicted.validate().is_err());
}

#[test]
fn target_selection_persists_its_exact_serde_shape() {
    let native: TargetSelection = serde_json::from_str("\"native\"").unwrap();
    assert_eq!(native, TargetSelection::Native);
    assert_eq!(
        serde_json::to_string(&TargetSelection::NativePriority).unwrap(),
        "\"native_priority\""
    );
    let openrouter = TargetSelection::OpenRouter {
        wire: WireBinding::resolve(ProtocolSelection::Messages),
        routing: OpenRouterRoutingSelection::GatewayManaged,
    };
    let encoded = serde_json::to_string(&openrouter).unwrap();
    assert_eq!(
        encoded,
        r#"{"open_router":{"wire":{"selection":"messages","resolved":"messages","resolution_revision":"plak-openrouter-wire-resolution-v1"},"routing":"gateway_managed"}}"#
    );
    let decoded: TargetSelection = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, openrouter);
    assert!(decoded.validate().is_ok());
    assert!(serde_json::from_str::<TargetSelection>(r#"{"open_router":{"wire":{"selection":"auto","resolved":"chat","resolution_revision":"plak-openrouter-wire-resolution-v1"},"routing":"gateway_managed","extra":1}}"#).is_err());
}
