use super::*;

#[test]
fn catalog_listing_preserves_literals_and_order_without_capability_claims()
-> Result<(), OpenRouterChatDiscoveryError> {
    let value = serde_json::json!({"data": [
        {"id":"vendor/z:free", "future_field": true}, {"id":"vendor/a"}, {"id":"~vendor/latest"}
    ]});
    let models = catalog_model_ids(&value)?;
    assert_eq!(
        models.iter().map(String::as_str).collect::<Vec<_>>(),
        ["vendor/z:free", "vendor/a", "~vendor/latest"]
    );
    assert_eq!(
        catalog_model_ids(&serde_json::json!({"data":[]}))?,
        [] as [String; 0]
    );
    assert_eq!(
        catalog_model_ids(&serde_json::json!({"data":[],"future":true}))?,
        [] as [String; 0]
    );
    for invalid in [
        serde_json::json!({"data":[{"id":"vendor/a"},{"id":"vendor/a"}]}),
        serde_json::json!({"data":[{"id":12}]}),
        serde_json::json!({"data":[{}]}),
        serde_json::json!({"data":{}}),
        serde_json::json!({"data": vec![serde_json::json!({"id":"vendor/a"}); MAX_MODELS + 1]}),
    ] {
        assert!(catalog_model_ids(&invalid).is_err());
    }
    assert!(parse_unique_json(br#"{"data":[],"data":[]}"#).is_err());
    Ok(())
}

use std::io;
use std::num::NonZeroU32;
use std::sync::Arc;

use plakous_http::{Limits, TlsConfigIdentity};
use plakous_model::{
    CachePolicy, CapacityCertificate, CapacityRule, ContextAdmissionRequirement, ContextContract,
    ContextEvidenceSource, CountCertificateKind, CredentialSelectionId, EvidenceFreshness,
    LanguageModel, LocalDurabilityDisposition, MeasurementCertificate, ModelAdapterProfileId,
    OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, ProtocolId,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderStorageDisposition, ReasoningEffortDisposition,
    ReasoningIncludeDisposition, ReasoningSummaryDisposition, ReplayDisposition,
    ResponseStateDisposition, RetentionDisposition, RuntimeModelAuthority,
    SelectedProviderConfiguration, ToolChoice, ToolPresentationMode,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::{
    CHAT_COMPLETIONS_FAMILY_ID, OpenRouterApiKey, OpenRouterChatLanguageModel,
    OpenRouterChatTarget, OpenRouterOrigin, OpenRouterRetentionPolicy,
    StaticOpenRouterApiKeySource,
};

fn custom_context(
    route: &OpenRouterChatRoute,
    maximum_projected_input: u64,
) -> Result<ContextContract, Box<dyn std::error::Error>> {
    let authority_binding = RuntimeModelAuthority::authority_class_for(
        route.identity().account_binding(),
        route.identity().credential_authority_binding(),
    )?;
    let declaration = Digest::sha256(
        DigestDomain::ProviderSemanticEvidence,
        1,
        &[
            b"test-caller/openrouter-chat-context-v1",
            route.identity().fingerprint().as_bytes(),
            &maximum_projected_input.to_be_bytes(),
        ],
    )?;
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, None)?;
    let revision = "test-caller-openrouter-chat-context-v1";
    let capacity = CapacityCertificate::custom_envelope(
        "test-caller-openrouter-chat-capacity",
        revision,
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(maximum_projected_input),
            maximum_projected_input,
            auto_compact_input: None,
        },
        authority_binding,
        freshness,
        declaration,
        revision,
    )?;
    let measurement = MeasurementCertificate::new(
        "test-caller-openrouter-chat-wire-bound",
        revision,
        "complete-lowered-chat-body-utf8-byte-upper-bound",
        revision,
        ContextEvidenceSource::CallerDeclaration,
        CountCertificateKind::CustomEnvelopeUpperBound { declaration },
        Some(authority_binding),
        freshness,
    )?;
    Ok(ContextContract::new(
        ProtocolId::new(CHAT_COMPLETIONS_FAMILY_ID)?,
        "openrouter-chat-custom-accounting-v1",
        "openrouter-chat-canonical-v1",
        "openrouter-chat-output-reservation-v1",
        capacity,
        vec![measurement],
    )?)
}

fn test_route() -> Result<OpenRouterChatRoute, Box<dyn std::error::Error>> {
    let source = Arc::new(StaticOpenRouterApiKeySource::new(
        OpenRouterApiKey::new("test-catalog-key")?,
        "test-account",
        "test-authority",
    )?);
    Ok(OpenRouterChatRoute::new_local_fixture_with_retention(
        source,
        OpenRouterOrigin::new("https://127.0.0.1:18443")?,
        OpenRouterRetentionPolicy::strict(),
    )?
    .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?))))
}

fn selected_for(
    target: &OpenRouterChatTarget,
) -> Result<SelectedProviderConfiguration, Box<dyn std::error::Error>> {
    let descriptor = target.descriptor();
    let revision = ProviderProfileRevisionId::new("openrouter-chat-custom-v1")?;
    Ok(SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            revision.clone(),
            ModelAdapterProfileId::new("openrouter-chat-api-key")?,
            ProviderProfileRevisionId::new("openrouter-chat-auth-v1")?,
            ProviderAuthorityClassId::new("custom-openrouter-chat")?,
            CredentialSelectionId::new("openrouter-chat-key")?,
            descriptor.requested_model().clone(),
        ),
        ProviderBehaviorConfiguration {
            processing_tier: plakous_model::ProcessingTierDisposition::Standard,
            tool_mode: ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Disabled,
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::Omit,
            replay: ReplayDisposition::DropUnreplayableReasoning,
            cache: CachePolicy::Disabled,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::None,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::Enforced(
                NonZeroU32::new(1024).ok_or("nonzero output")?,
            ),
            storage: ProviderStorageDisposition::DisabledOnWire,
            retention: RetentionDisposition::RouteContract {
                profile: descriptor.profile().clone(),
                revision,
            },
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::Ephemeral,
        },
    )?)
}

#[test]
fn literal_paths_are_exact_and_variants_are_encoded_without_privilege()
-> Result<(), Box<dyn std::error::Error>> {
    let (author, slug, model) = exact_literal("vendor/model-v1:free")?;
    assert_eq!(author, "vendor");
    assert_eq!(model.as_str(), "vendor/model-v1:free");
    assert_eq!(encode_path_component(slug), "model-v1%3Afree");
    let (_, slug, _) = exact_literal("vendor/model-v1:any-variant")?;
    assert_eq!(encode_path_component(slug), "model-v1%3Aany-variant");
    for invalid in ["vendor/a/b", "Vendor/model", "./model", "vendor/.."] {
        assert!(exact_literal(invalid).is_err());
    }
    Ok(())
}

#[test]
fn duplicate_and_unknown_json_fail_closed() {
    assert!(parse_unique_json(br#"{"data":[],"data":[]}"#).is_err());
    match parse_unique_json(b"{") {
        Err(OpenRouterChatDiscoveryError::InvalidJson) => {}
        other => panic!("expected named catalog JSON, got {other:?}"),
    }
    assert!(
        OpenRouterChatDiscoveryError::UnexpectedStatus
            .to_string()
            .contains("unexpected HTTP status")
    );
    assert!(
        OpenRouterChatDiscoveryError::UnexpectedMediaType
            .to_string()
            .contains("unexpected media type")
    );
    let not_object = serde_json::json!([]);
    match validate_models_document(&not_object, "vendor/model") {
        Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(path)) => {
            assert_eq!(path, "models");
        }
        other => panic!(
            "expected named catalog value, got {}",
            match other {
                Ok(_) => "ok",
                Err(_) => "other error",
            }
        ),
    }
    let evolving_neighbor = serde_json::json!({"data": [
        {"id": "other/model", "future_neighbor_field": true},
        {"id": "vendor/model", "canonical_slug": "vendor/model", "context_length": 1024,
         "supported_parameters": ["tools"]}
    ]});
    assert!(validate_models_document(&evolving_neighbor, "vendor/model").is_ok());
    let evolving_selected = serde_json::json!({"data": [
        {"id": "vendor/model", "canonical_slug": "vendor/model", "context_length": 1024,
         "supported_parameters": ["tools"], "future_selected_field": true}
    ]});
    assert!(validate_models_document(&evolving_selected, "vendor/model").is_ok());
    let variant = serde_json::json!({"data": [
        {"id": "vendor/model:free", "canonical_slug": "vendor/model-20260824",
         "context_length": 1024, "supported_parameters": ["tools"]}
    ]});
    assert!(validate_models_document(&variant, "vendor/model:free").is_ok());
    assert!(matches!(
        validate_models_document(&evolving_neighbor, "vendor/absent"),
        Err(OpenRouterChatDiscoveryError::AbsentModel)
    ));
    let duplicated = serde_json::json!({"data": [
        {"id": "vendor/model", "canonical_slug": "vendor/model", "context_length": 1024,
         "supported_parameters": ["tools"]},
        {"id": "vendor/model", "canonical_slug": "vendor/model", "context_length": 1024,
         "supported_parameters": ["tools"]}
    ]});
    assert!(matches!(
        validate_models_document(&duplicated, "vendor/model"),
        Err(OpenRouterChatDiscoveryError::AmbiguousModel)
    ));
}

#[test]
fn endpoints_parse_supports_tool_choice_capability_object() -> Result<(), Box<dyn std::error::Error>>
{
    let endpoint = |tool_choice: Value| {
        serde_json::json!({"data":{"id":"vendor/model","endpoints":[{
            "tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],
            "supports_tool_choice": tool_choice
        }]}})
    };
    let parsed = validate_endpoints_document(
        &endpoint(serde_json::json!({
            "none": true,
            "auto": false,
            "required": true,
            "function": false
        })),
        "vendor/model",
    )?;
    let candidate = &parsed[0];
    assert_eq!(
        candidate.tool_choice(),
        OpenRouterToolChoiceModes {
            none: true,
            auto: false,
            required: true,
            function: false,
        }
    );

    let missing_keys =
        validate_endpoints_document(&endpoint(serde_json::json!({"auto": true})), "vendor/model")?;
    assert_eq!(
        missing_keys[0].tool_choice(),
        OpenRouterToolChoiceModes {
            none: false,
            auto: true,
            required: false,
            function: false,
        }
    );

    let absent = validate_endpoints_document(
        &serde_json::json!({"data":{"id":"vendor/model","endpoints":[{
            "tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"]
        }]}}),
        "vendor/model",
    )?;
    assert_eq!(
        absent[0].tool_choice(),
        OpenRouterToolChoiceModes::default()
    );

    match validate_endpoints_document(&endpoint(serde_json::json!(true)), "vendor/model") {
        Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(path)) => {
            assert_eq!(path, "endpoints.data.endpoints[].supports_tool_choice");
        }
        other => panic!("expected boolean rejection, got {other:?}"),
    }

    let future_mode = validate_endpoints_document(
        &endpoint(serde_json::json!({"none": true, "future_mode": true})),
        "vendor/model",
    )?;
    assert_eq!(
        future_mode[0].tool_choice(),
        OpenRouterToolChoiceModes {
            none: true,
            ..OpenRouterToolChoiceModes::default()
        }
    );
    Ok(())
}

#[test]
fn models_read_image_input_from_catalog_architecture() -> Result<(), Box<dyn std::error::Error>> {
    let model = |architecture: Option<Value>| {
        let mut row = serde_json::json!({
            "id": "qwen/qwen3.7-flash",
            "canonical_slug": "qwen/qwen3.7-flash-20260727",
            "context_length": 1_000_000,
            "supported_parameters": ["tools", "max_tokens"]
        });
        if let Some(architecture) = architecture {
            row["architecture"] = architecture;
        }
        serde_json::json!({"data": [row]})
    };
    // Shape of the live catalog row.
    let vision = model(Some(serde_json::json!({
        "modality": "text+image+video->text",
        "input_modalities": ["text", "image", "video"],
        "output_modalities": ["text"],
        "tokenizer": "Qwen",
        "instruct_type": null
    })));
    assert!(validate_models_document(&vision, "qwen/qwen3.7-flash")?.image_input);
    let text_only = model(Some(serde_json::json!({
        "modality": "text->text",
        "input_modalities": ["text"],
        "output_modalities": ["text"]
    })));
    assert!(!validate_models_document(&text_only, "qwen/qwen3.7-flash")?.image_input);
    assert!(!validate_models_document(&model(None), "qwen/qwen3.7-flash")?.image_input);
    assert!(
        validate_models_document(
            &model(Some(serde_json::json!({"input_modalities": "image"}))),
            "qwen/qwen3.7-flash"
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn models_parse_reasoning_capability_object() -> Result<(), Box<dyn std::error::Error>> {
    let model = |reasoning: Option<Value>| {
        let mut row = serde_json::json!({
            "id": "vendor/model",
            "canonical_slug": "vendor/model",
            "context_length": 4096,
            "supported_parameters": ["tools"]
        });
        if let Some(reasoning) = reasoning {
            row["reasoning"] = reasoning;
        }
        serde_json::json!({"data": [row]})
    };

    let with_efforts = validate_models_document(
        &model(Some(serde_json::json!({
            "supported_efforts": ["xhigh", "high", "medium", "low", "minimal", "none"],
            "default_effort": "medium",
            "default_enabled": true,
            "supports_max_tokens": false,
            "mandatory": false
        }))),
        "vendor/model",
    )?;
    let reasoning = with_efforts
        .reasoning
        .as_ref()
        .ok_or("reasoning object should be present")?;
    assert_eq!(
        reasoning.supported_efforts(),
        Some(
            [
                "xhigh".to_owned(),
                "high".to_owned(),
                "medium".to_owned(),
                "low".to_owned(),
                "minimal".to_owned(),
                "none".to_owned(),
            ]
            .as_slice()
        )
    );
    assert_eq!(reasoning.default_effort.as_deref(), Some("medium"));
    assert!(reasoning.default_enabled());
    assert!(!reasoning.supports_max_tokens);
    assert!(!reasoning.mandatory());

    let max_tokens_only = validate_models_document(
        &model(Some(serde_json::json!({
            "supports_max_tokens": true,
            "default_enabled": true,
            "mandatory": false
        }))),
        "vendor/model",
    )?;
    let reasoning = max_tokens_only
        .reasoning
        .as_ref()
        .ok_or("reasoning object should be present")?;
    assert!(reasoning.supported_efforts().is_none());
    assert!(reasoning.supports_max_tokens);

    let absent = validate_models_document(&model(None), "vendor/model")?;
    assert!(absent.reasoning.is_none());

    let unknown_only = validate_models_document(
        &model(Some(serde_json::json!({"supported": true}))),
        "vendor/model",
    )?;
    let reasoning = unknown_only
        .reasoning
        .as_ref()
        .ok_or("reasoning object should be present")?;
    assert!(reasoning.supported_efforts().is_none());
    assert!(!reasoning.supports_max_tokens);

    let too_many = (0..17)
        .map(|index| format!("effort-{index}"))
        .collect::<Vec<_>>();
    assert!(matches!(
        validate_models_document(
            &model(Some(serde_json::json!({"supported_efforts": too_many}))),
            "vendor/model",
        ),
        Err(OpenRouterChatDiscoveryError::ResourceLimit)
    ));

    assert!(matches!(
        validate_models_document(
            &model(Some(serde_json::json!({"supported_efforts": ["high", "high"]}))),
            "vendor/model",
        ),
        Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(path))
            if path == "models.data[].reasoning.supported_efforts[]"
    ));
    Ok(())
}

#[test]
fn snapshot_from_facts_rotates_on_tool_choice_drift() -> Result<(), Box<dyn std::error::Error>> {
    let models = parse_unique_json(
        br#"{"data":[{"id":"vendor/model","canonical_slug":"vendor/model","context_length":4096,"supported_parameters":["tools"]}]}"#,
    )?;
    let none_true = parse_unique_json(
        br#"{"data":{"id":"vendor/model","endpoints":[{"tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],"supports_tool_choice":{"none":true,"auto":true,"required":false,"function":false}}]}}"#,
    )?;
    let none_false = parse_unique_json(
        br#"{"data":{"id":"vendor/model","endpoints":[{"tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],"supports_tool_choice":{"none":false,"auto":true,"required":false,"function":false}}]}}"#,
    )?;
    let facts = validate_models_document(&models, "vendor/model")?;
    let with_none = validate_endpoints_document(&none_true, "vendor/model")?;
    let without_none = validate_endpoints_document(&none_false, "vendor/model")?;
    let route = "stable-route";
    let first = snapshot_from_facts(route, "vendor/model", &facts, &with_none)?;
    let drifted = snapshot_from_facts(route, "vendor/model", &facts, &without_none)?;
    assert_ne!(first, drifted);
    Ok(())
}

#[test]
fn snapshot_from_facts_rotates_on_reasoning_and_endpoint_capability_drift()
-> Result<(), Box<dyn std::error::Error>> {
    let base_models = parse_unique_json(
        br#"{"data":[{"id":"vendor/model","canonical_slug":"vendor/model","context_length":4096,"supported_parameters":["tools"]}]}"#,
    )?;
    let base_endpoints = parse_unique_json(
        br#"{"data":{"id":"vendor/model","endpoints":[{"tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],"supports_implicit_caching":false}]}}"#,
    )?;
    let route = "stable-route";
    let base_facts = validate_models_document(&base_models, "vendor/model")?;
    let base_candidates = validate_endpoints_document(&base_endpoints, "vendor/model")?;
    let base = snapshot_from_facts(route, "vendor/model", &base_facts, &base_candidates)?;

    let model_with_reasoning = |reasoning: Value| {
        serde_json::json!({
            "data": [{
                "id": "vendor/model",
                "canonical_slug": "vendor/model",
                "context_length": 4096,
                "supported_parameters": ["tools"],
                "reasoning": reasoning
            }]
        })
    };
    for reasoning in [
        serde_json::json!({"supported_efforts": ["low", "high"]}),
        serde_json::json!({"supported_efforts": ["high", "low"]}),
        serde_json::json!({"default_effort": "medium"}),
        serde_json::json!({"default_effort": "low"}),
        serde_json::json!({"default_enabled": true}),
        serde_json::json!({"default_enabled": false}),
        serde_json::json!({"supports_max_tokens": true}),
        serde_json::json!({"supports_max_tokens": false}),
        serde_json::json!({"mandatory": true}),
        serde_json::json!({"mandatory": false}),
    ] {
        let facts = validate_models_document(&model_with_reasoning(reasoning), "vendor/model")?;
        let identity = snapshot_from_facts(route, "vendor/model", &facts, &base_candidates)?;
        assert_ne!(base, identity);
    }

    let caching_endpoint = serde_json::json!({"data":{"id":"vendor/model","endpoints":[{
        "tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],
        "supports_implicit_caching": true
    }]}});
    let candidates = validate_endpoints_document(&caching_endpoint, "vendor/model")?;
    let identity = snapshot_from_facts(route, "vendor/model", &base_facts, &candidates)?;
    assert_ne!(base, identity);
    Ok(())
}

#[test]
fn duplicate_endpoints_coalesce_only_equal_authority() -> Result<(), Box<dyn std::error::Error>> {
    let base = serde_json::json!({
        "tag": "b/model", "provider_name": "Beta",
        "supported_parameters": ["tools", "max_tokens"],
        "context_length": 4096, "max_prompt_tokens": 3072,
        "max_completion_tokens": 1024,
        "supports_tool_choice": {"auto": true},
        "supports_implicit_caching": false,
        "uptime_last_1d": 99.1
    });
    let document = |rows: Vec<Value>| {
        serde_json::json!({
            "data": {"id": "vendor/model", "endpoints": rows}
        })
    };
    let mut duplicate = base.clone();
    duplicate["uptime_last_1d"] = serde_json::json!(99.9);
    duplicate["supported_parameters"] = serde_json::json!(["max_tokens", "tools"]);
    let mut other = base.clone();
    other["tag"] = serde_json::json!("a/model");
    let expected =
        validate_endpoints_document(&document(vec![other.clone(), base.clone()]), "vendor/model")?;
    assert_eq!(expected[0].endpoint(), "a/model");
    assert_eq!(expected[1].endpoint(), "b/model");
    for rows in [
        vec![base.clone(), other.clone(), duplicate.clone()],
        vec![duplicate.clone(), base.clone(), other.clone()],
    ] {
        assert_eq!(
            validate_endpoints_document(&document(rows), "vendor/model")?,
            expected
        );
    }
    for (field, conflict) in [
        ("provider_name", serde_json::json!("Other")),
        ("supported_parameters", serde_json::json!(["tools"])),
        ("context_length", serde_json::json!(8192)),
        ("max_prompt_tokens", serde_json::json!(2048)),
        ("max_completion_tokens", serde_json::json!(512)),
        ("supports_tool_choice", serde_json::json!({"auto": false})),
        ("supports_implicit_caching", serde_json::json!(true)),
    ] {
        let mut changed = base.clone();
        changed[field] = conflict;
        for rows in [
            vec![base.clone(), changed.clone()],
            vec![changed, base.clone()],
        ] {
            assert!(
                matches!(
                    validate_endpoints_document(&document(rows), "vendor/model"),
                    Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(_))
                ),
                "conflicting {field}"
            );
        }
    }
    Ok(())
}

fn endpoint_document(rows: Vec<Value>) -> Value {
    serde_json::json!({"data": {"id": "vendor/model", "endpoints": rows}})
}

fn endpoint_tags(candidates: &[OpenRouterChatEndpointCandidate]) -> Vec<String> {
    candidates
        .iter()
        .map(|candidate| candidate.endpoint().to_owned())
        .collect()
}

struct DuplicateEndpointFixture {
    kept: Value,
    kept_repeat: Value,
    other: Value,
    base_conflict: Value,
    facts: ModelFacts,
    clean_identity: Digest,
}

impl DuplicateEndpointFixture {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let kept = serde_json::json!({
            "tag": "a/model", "provider_name": "Alpha",
            "supported_parameters": ["tools", "max_tokens"],
            "context_length": 8192,
            "max_prompt_tokens": 6144,
            "max_completion_tokens": 2048,
            "supports_tool_choice": {"auto": true},
            "supports_implicit_caching": false,
            "uptime_last_30m": 98.0
        });
        let mut kept_repeat = kept.clone();
        kept_repeat["uptime_last_30m"] = serde_json::json!(97.0);
        kept_repeat["supported_parameters"] = serde_json::json!(["max_tokens", "tools"]);
        let other = serde_json::json!({
            "tag": "z/model", "provider_name": "Zed",
            "supported_parameters": ["tools"]
        });
        let base_conflict = serde_json::json!({
            "tag": "baseten/fp8", "provider_name": "Baseten",
            "supported_parameters": ["tools", "response_format"],
            "context_length": 4096,
            "max_prompt_tokens": 3072,
            "max_completion_tokens": 1024,
            "supports_tool_choice": {"auto": true, "none": true},
            "supports_implicit_caching": false
        });
        let facts = ModelFacts {
            supported_parameters: BTreeSet::from(["tools".to_owned()]),
            reasoning: None,
            context_length: 8192,
            image_input: false,
        };
        let admitted_without_conflict = validate_endpoints_document(
            &endpoint_document(vec![other.clone(), kept.clone(), kept_repeat.clone()]),
            "vendor/model",
        )?;
        assert_eq!(
            endpoint_tags(&admitted_without_conflict),
            ["a/model", "z/model"]
        );
        let clean_identity = snapshot_from_facts(
            "stable-route",
            "vendor/model",
            &facts,
            &admitted_without_conflict,
        )?;

        Ok(Self {
            kept,
            kept_repeat,
            other,
            base_conflict,
            facts,
            clean_identity,
        })
    }

    fn orders(&self, changed: &Value) -> [Vec<Value>; 3] {
        // A later matching row must not resurrect a conflicted tag.
        let mut majority = self.base_conflict.clone();
        majority["uptime_last_30m"] = serde_json::json!(50.0);
        [
            vec![
                self.base_conflict.clone(),
                changed.clone(),
                self.kept.clone(),
                self.other.clone(),
                self.kept_repeat.clone(),
                majority.clone(),
            ],
            vec![
                changed.clone(),
                self.other.clone(),
                majority.clone(),
                self.kept_repeat.clone(),
                self.base_conflict.clone(),
                self.kept.clone(),
            ],
            vec![
                self.kept_repeat.clone(),
                majority,
                self.other.clone(),
                changed.clone(),
                self.kept.clone(),
                self.base_conflict.clone(),
            ],
        ]
    }
}

#[test]
fn ambiguous_duplicate_tags_drop_only_that_endpoint() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = DuplicateEndpointFixture::new()?;
    for (field, conflict) in [
        ("provider_name", serde_json::json!("Other")),
        ("supported_parameters", serde_json::json!(["tools"])),
        ("context_length", serde_json::json!(8192)),
        ("max_prompt_tokens", serde_json::json!(2048)),
        ("max_completion_tokens", serde_json::json!(512)),
        ("supports_tool_choice", serde_json::json!({"auto": false})),
        ("supports_implicit_caching", serde_json::json!(true)),
    ] {
        let mut changed = fixture.base_conflict.clone();
        changed[field] = conflict;
        let orders = fixture.orders(&changed);
        for rows in orders {
            let candidates = validate_endpoints_document(&endpoint_document(rows), "vendor/model")?;
            assert_eq!(
                endpoint_tags(&candidates),
                ["a/model", "z/model"],
                "conflicting {field}"
            );
            let identity =
                snapshot_from_facts("stable-route", "vendor/model", &fixture.facts, &candidates)?;
            assert_eq!(identity, fixture.clean_identity, "conflicting {field}");
        }
        for winner in [fixture.base_conflict.clone(), changed] {
            let picked = validate_endpoints_document(
                &endpoint_document(vec![winner, fixture.kept.clone(), fixture.other.clone()]),
                "vendor/model",
            )?;
            let picked_identity =
                snapshot_from_facts("stable-route", "vendor/model", &fixture.facts, &picked)?;
            assert_ne!(
                picked_identity, fixture.clean_identity,
                "winner for {field}"
            );
        }
    }

    Ok(())
}

#[test]
fn duplicate_tag_boundary_rows_are_exact() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = DuplicateEndpointFixture::new()?;
    let mut changed = fixture.base_conflict.clone();
    changed["supported_parameters"] = serde_json::json!(["tools"]);
    let mut second = fixture.other.clone();
    second["provider_name"] = serde_json::json!("OtherZed");
    let mut unsafe_row = fixture.kept_repeat.clone();
    unsafe_row["tag"] = serde_json::json!("bad tag");
    for (label, rows, expected) in [
        (
            "all conflicting",
            vec![fixture.base_conflict.clone(), changed.clone()],
            "tag",
        ),
        (
            "all conflicting reversed",
            vec![changed.clone(), fixture.base_conflict.clone()],
            "tag",
        ),
        (
            "two conflicted tags",
            vec![
                fixture.base_conflict,
                changed,
                second,
                fixture.other,
                fixture.kept.clone(),
            ],
            "a/model",
        ),
        (
            "malformed duplicate row",
            vec![fixture.kept, unsafe_row, fixture.kept_repeat],
            "invalid",
        ),
    ] {
        let result = validate_endpoints_document(&endpoint_document(rows), "vendor/model");
        match expected {
            "tag" => assert!(
                matches!(result, Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(ref path)) if path == "endpoints.data.endpoints[].tag"),
                "{label}: {result:?}"
            ),
            "invalid" => assert!(
                matches!(
                    result,
                    Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(_))
                ),
                "{label}: {result:?}"
            ),
            tag => assert_eq!(endpoint_tags(&result?), [tag], "{label}"),
        }
    }
    Ok(())
}

#[test]
fn duplicate_endpoints_still_validate_every_row() {
    let base = serde_json::json!({
        "tag": "a/model", "provider_name": "Alpha", "supported_parameters": ["tools"]
    });
    for (field, malformed) in [
        ("supported_parameters", serde_json::json!("tools")),
        ("max_completion_tokens", serde_json::json!(0)),
        ("supports_tool_choice", serde_json::json!({"auto": "true"})),
        ("supports_implicit_caching", serde_json::json!("false")),
    ] {
        let mut duplicate = base.clone();
        duplicate[field] = malformed;
        for rows in [
            vec![base.clone(), duplicate.clone()],
            vec![duplicate, base.clone()],
        ] {
            let document = serde_json::json!({"data": {"id": "vendor/model", "endpoints": rows}});
            assert!(
                validate_endpoints_document(&document, "vendor/model").is_err(),
                "malformed {field}"
            );
        }
    }
}

#[test]
fn endpoints_admit_observed_supports_voice_cloning() {
    let ok = serde_json::json!({"data":{"id":"vendor/model","endpoints":[{
        "tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],
        "supports_voice_cloning": true
    }]}});
    assert!(validate_endpoints_document(&ok, "vendor/model").is_ok());
}

#[test]
fn additive_upstream_fields_are_ignored_but_known_fields_stay_checked()
-> Result<(), Box<dyn std::error::Error>> {
    // Shape of the endpoints document that first carried
    // `supports_image_reference`, plus fields at every nesting level.
    let document = serde_json::json!({"future_root": 1, "data": {
        "id": "vendor/model", "future_data": {"x": [1]},
        "endpoints": [{
            "tag": "a/model", "provider_name": "Alpha",
            "supported_parameters": ["tools", "tool_choice"],
            "context_length": 4096,
            "supports_image_reference": false,
            "supports_tool_choice": {"auto": true, "future_mode": true},
            "future_endpoint_field": {"nested": true}
        }]
    }});
    let endpoints = validate_endpoints_document(&document, "vendor/model")?;
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].context_length, Some(4096));
    assert!(endpoints[0].tool_choice().auto);

    let models = serde_json::json!({"future_root": true, "data": [{
        "id": "vendor/model", "canonical_slug": "vendor/model", "context_length": 1024,
        "supported_parameters": ["tools"], "future_selected_field": {"a": 1},
        "reasoning": {"supported_efforts": ["low"], "future_reasoning": 1}
    }]});
    let facts = validate_models_document(&models, "vendor/model")?;
    assert_eq!(facts.context_length, 1024);
    assert_eq!(catalog_model_ids(&models)?, vec!["vendor/model".to_owned()]);

    // A known field with the wrong type is still refused.
    let mut wrong_type = document.clone();
    wrong_type["data"]["endpoints"][0]["context_length"] = serde_json::json!("4096");
    assert!(matches!(
        validate_endpoints_document(&wrong_type, "vendor/model"),
        Err(OpenRouterChatDiscoveryError::InvalidCatalogValue(_))
    ));
    Ok(())
}

#[test]
fn endpoints_admit_observed_perf_last_30m_by_workload() {
    let ok = serde_json::json!({"data":{"id":"vendor/model","endpoints":[{
        "tag":"a/model","provider_name":"Alpha","supported_parameters":["tools"],
        "perf_last_30m_by_workload":{"chat":{"p50":1.0}}
    }]}});
    assert!(validate_endpoints_document(&ok, "vendor/model").is_ok());
}

#[test]
fn snapshot_from_facts_ignores_raw_receipt_shape_and_rotates_on_drift()
-> Result<(), Box<dyn std::error::Error>> {
    let first_models = parse_unique_json(
        br#"{"data":[{"id":"vendor/model","canonical_slug":"vendor/model","context_length":4096,"supported_parameters":["tool_choice","tools"]}],"links":{},"total_count":1}"#,
    )?;
    let reordered_models = parse_unique_json(
        br#"{ "total_count": 1, "data": [ { "supported_parameters": ["tools", "tool_choice"], "context_length": 4096, "canonical_slug": "vendor/model", "id": "vendor/model" } ], "links": {} }"#,
    )?;
    let endpoints = parse_unique_json(
        br#"{"data":{"id":"vendor/model","endpoints":[{"tag":"z/model","provider_name":"Zed","supported_parameters":["tools"]},{"tag":"a/model","provider_name":"Alpha","supported_parameters":["tools","tool_choice"]}]}}"#,
    )?;
    let reordered_endpoints = parse_unique_json(
        br#"{"data":{"endpoints":[{"supported_parameters":["tool_choice","tools"],"provider_name":"Alpha","tag":"a/model"},{"supported_parameters":["tools"],"provider_name":"Zed","tag":"z/model"}],"id":"vendor/model"}}"#,
    )?;
    let first_facts = validate_models_document(&first_models, "vendor/model")?;
    let reordered_facts = validate_models_document(&reordered_models, "vendor/model")?;
    let candidates = validate_endpoints_document(&endpoints, "vendor/model")?;
    let reordered_candidates = validate_endpoints_document(&reordered_endpoints, "vendor/model")?;
    let route = "stable-route";
    let first = snapshot_from_facts(route, "vendor/model", &first_facts, &candidates)?;
    let reordered = snapshot_from_facts(route, "vendor/model", &reordered_facts, &candidates)?;
    assert_eq!(first, reordered);
    let endpoint_reordered = snapshot_from_facts(
        route,
        "vendor/model",
        &reordered_facts,
        &reordered_candidates,
    )?;
    assert_eq!(first, endpoint_reordered);

    let drifted_models = parse_unique_json(
        br#"{"data":[{"id":"vendor/model","canonical_slug":"vendor/model","context_length":4095,"supported_parameters":["tools","tool_choice"]}]}"#,
    )?;
    let drifted_facts = validate_models_document(&drifted_models, "vendor/model")?;
    let drifted = snapshot_from_facts(route, "vendor/model", &drifted_facts, &candidates)?;
    assert_ne!(first, drifted);
    Ok(())
}

#[test]
fn chat_configuration_rejects_explicit_tool_choice() -> Result<(), Box<dyn std::error::Error>> {
    let route = test_route()?;
    let now = plakous_model::FixedClock::new(Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)
    .now();
    let evidence = OpenRouterChatModelEvidence::from_authenticated_discovery(
        ModelId::new("vendor/model")?,
        route.identity(),
        crate::chat::OpenRouterChatDiscovery {
            endpoints: vec![OpenRouterChatEndpointCandidate {
                endpoint: "provider/model".to_owned(),
                provider_name: "Provider".to_owned(),
                supported_parameters: BTreeSet::from(
                    ["tools", "max_tokens", "tool_choice"].map(str::to_owned),
                ),
                context_length: Some(4096),
                max_prompt_tokens: Some(3072),
                max_completion_tokens: Some(1024),
                tool_choice: OpenRouterToolChoiceModes {
                    required: true,
                    ..OpenRouterToolChoiceModes::default()
                },
                supports_implicit_caching: false,
            }],
            model_supported_parameters: BTreeSet::from(
                ["tools", "max_tokens", "tool_choice"].map(str::to_owned),
            ),
            reasoning: None,
            context_length: 4096,
            image_input: false,
            observed_at: now,
            expires_at: Timestamp::new(now.seconds() + 3600, now.nanoseconds())?,
        },
    )?;
    let contract = custom_context(&route, 2048)?;
    let target =
        OpenRouterChatTarget::gateway_managed(route.clone(), evidence.clone(), contract.clone())?;
    let selected = selected_for(&target)?;
    let mut behavior = selected.behavior().clone();
    behavior.tool_choice = ToolChoice::Required;
    let selected = SelectedProviderConfiguration::new(selected.route().clone(), behavior)?;
    assert!(matches!(
        OpenRouterChatLanguageModel::configured_gateway_managed(
            route,
            evidence,
            contract,
            selected,
            Client::new(Limits::default())?
        ),
        Err(OpenRouterChatBuildError::Configuration)
    ));
    Ok(())
}

#[test]
fn time_only_rediscovery_preserves_target_and_effective_configuration_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let route = test_route()?;
    let model = ModelId::new("vendor/model")?;
    let candidates = vec![OpenRouterChatEndpointCandidate {
        endpoint: "provider/model".to_owned(),
        provider_name: "Provider".to_owned(),
        supported_parameters: BTreeSet::from(["tools".to_owned()]),
        context_length: Some(4096),
        max_prompt_tokens: Some(3072),
        max_completion_tokens: Some(1024),
        tool_choice: OpenRouterToolChoiceModes::default(),
        supports_implicit_caching: false,
    }];
    let parameters = BTreeSet::from(["tools".to_owned()]);
    let now = plakous_model::FixedClock::new(Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)
    .now();
    let first_observed = Timestamp::new(now.seconds().saturating_sub(20), now.nanoseconds())?;
    let second_observed = Timestamp::new(now.seconds().saturating_sub(10), now.nanoseconds())?;
    let first_expiry = Timestamp::new(now.seconds() + 3600, now.nanoseconds())?;
    let second_expiry = Timestamp::new(now.seconds() + 7200, now.nanoseconds())?;
    let first_evidence = OpenRouterChatModelEvidence::from_authenticated_discovery(
        model.clone(),
        route.identity(),
        crate::chat::OpenRouterChatDiscovery {
            endpoints: candidates.clone(),
            model_supported_parameters: parameters.clone(),
            reasoning: None,
            context_length: 4096,
            image_input: false,
            observed_at: first_observed,
            expires_at: first_expiry,
        },
    )?;
    let second_evidence = OpenRouterChatModelEvidence::from_authenticated_discovery(
        model,
        route.identity(),
        crate::chat::OpenRouterChatDiscovery {
            endpoints: candidates,
            model_supported_parameters: parameters,
            reasoning: None,
            context_length: 4096,
            image_input: false,
            observed_at: second_observed,
            expires_at: second_expiry,
        },
    )?;
    let context = custom_context(&route, 2048)?;
    let first_target = OpenRouterChatTarget::gateway_managed(
        route.clone(),
        first_evidence.clone(),
        context.clone(),
    )?;
    let second_target = OpenRouterChatTarget::gateway_managed(
        route.clone(),
        second_evidence.clone(),
        context.clone(),
    )?;
    assert_eq!(
        first_target.target_identity(),
        second_target.target_identity()
    );
    assert_eq!(first_target.descriptor(), second_target.descriptor());
    let selected = selected_for(&first_target)?;
    let first = OpenRouterChatLanguageModel::configured_gateway_managed(
        route.clone(),
        first_evidence,
        context.clone(),
        selected.clone(),
        Client::new(Limits::default())?,
    )?;
    let second = OpenRouterChatLanguageModel::configured_gateway_managed(
        route,
        second_evidence,
        context,
        selected,
        Client::new(Limits::default())?,
    )?;
    assert_eq!(
        first.configuration().effective_identity(),
        second.configuration().effective_identity()
    );
    Ok(())
}

#[test]
fn equal_endpoint_catalog_drift_preserves_chat_admission() -> Result<(), Box<dyn std::error::Error>>
{
    let route = test_route()?;
    let document = serde_json::json!({"data": {"id": "vendor/model", "endpoints": [{
        "tag": "first/model", "provider_name": "First",
        "supported_parameters": ["tools", "max_tokens"],
        "context_length": 4096, "max_prompt_tokens": 3072,
        "max_completion_tokens": 1024
    }]}});
    let mut added = document.clone();
    let mut row = document["data"]["endpoints"][0].clone();
    row["tag"] = serde_json::json!("second/model");
    row["provider_name"] = serde_json::json!("Second");
    added["data"]["endpoints"]
        .as_array_mut()
        .ok_or("rows")?
        .push(row);
    let facts = ModelFacts {
        supported_parameters: BTreeSet::from(["tools".to_owned(), "max_tokens".to_owned()]),
        reasoning: None,
        context_length: 4096,
        image_input: false,
    };
    let now = plakous_model::FixedClock::new(Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)
    .now();
    let mut identities = Vec::new();
    let mut snapshots = Vec::new();
    for document in [&document, &added, &document] {
        let candidates = validate_endpoints_document(document, "vendor/model")?;
        let snapshot = snapshot_from_facts(
            route.identity().fingerprint(),
            "vendor/model",
            &facts,
            &candidates,
        )?;
        snapshots.push(snapshot);
        let evidence = OpenRouterChatModelEvidence::from_authenticated_discovery(
            ModelId::new("vendor/model")?,
            route.identity(),
            crate::chat::OpenRouterChatDiscovery {
                endpoints: candidates,
                model_supported_parameters: facts.supported_parameters.clone(),
                reasoning: None,
                context_length: 4096,
                image_input: false,
                observed_at: now,
                expires_at: Timestamp::new(now.seconds() + 3600, now.nanoseconds())?,
            },
        )?;
        let target = OpenRouterChatTarget::gateway_managed(
            route.clone(),
            evidence,
            custom_context(&route, 2048)?,
        )?;
        identities.push(target.target_identity());
    }
    assert_ne!(snapshots[0], snapshots[1]);
    assert_eq!(snapshots[0], snapshots[2]);
    assert_eq!(identities[0], identities[1]);
    assert_eq!(identities[0], identities[2]);
    Ok(())
}

#[test]
fn discovery_constrains_but_does_not_own_custom_context() -> Result<(), Box<dyn std::error::Error>>
{
    let route = test_route()?;
    let now = plakous_model::FixedClock::new(Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)
    .now();
    let evidence = OpenRouterChatModelEvidence::from_authenticated_discovery(
        ModelId::new("vendor/model")?,
        route.identity(),
        crate::chat::OpenRouterChatDiscovery {
            endpoints: vec![OpenRouterChatEndpointCandidate {
                endpoint: "provider/model".to_owned(),
                provider_name: "Provider".to_owned(),
                supported_parameters: BTreeSet::new(),
                context_length: Some(4096),
                max_prompt_tokens: Some(3072),
                max_completion_tokens: Some(1024),
                tool_choice: OpenRouterToolChoiceModes::default(),
                supports_implicit_caching: false,
            }],
            model_supported_parameters: BTreeSet::new(),
            reasoning: None,
            context_length: 4096,
            image_input: false,
            observed_at: Timestamp::new(now.seconds().saturating_sub(1), now.nanoseconds())?,
            expires_at: Timestamp::new(now.seconds() + 3600, now.nanoseconds())?,
        },
    )?;
    let conservative = custom_context(&route, 2048)?;
    let target = OpenRouterChatTarget::gateway_managed(
        route.clone(),
        evidence.clone(),
        conservative.clone(),
    )?;
    assert_eq!(target.descriptor().context_contract(), Some(&conservative));
    let excessive = custom_context(&route, 4097)?;
    assert!(matches!(
        OpenRouterChatTarget::gateway_managed(route, evidence, excessive),
        Err(OpenRouterChatBuildError::ContextExceedsDiscovery)
    ));
    Ok(())
}

#[tokio::test]
async fn exact_discovery_freshness_gates_construction_and_runtime_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let route = test_route()?;
    let context = custom_context(&route, 2048)?;
    let now = plakous_model::FixedClock::new(Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)
    .now();
    let candidate = OpenRouterChatEndpointCandidate {
        endpoint: "provider/model".to_owned(),
        provider_name: "Provider".to_owned(),
        supported_parameters: BTreeSet::new(),
        context_length: Some(4096),
        max_prompt_tokens: Some(3072),
        max_completion_tokens: Some(1024),
        tool_choice: OpenRouterToolChoiceModes::default(),
        supports_implicit_caching: false,
    };
    let stale = OpenRouterChatModelEvidence::from_authenticated_discovery(
        ModelId::new("vendor/model")?,
        route.identity(),
        crate::chat::OpenRouterChatDiscovery {
            endpoints: vec![candidate.clone()],
            model_supported_parameters: BTreeSet::new(),
            reasoning: None,
            context_length: 4096,
            image_input: false,
            observed_at: Timestamp::new(now.seconds().saturating_sub(20), now.nanoseconds())?,
            expires_at: Timestamp::new(now.seconds().saturating_sub(10), now.nanoseconds())?,
        },
    )?;
    assert!(matches!(
        OpenRouterChatTarget::gateway_managed(route.clone(), stale, context.clone()),
        Err(OpenRouterChatBuildError::StaleEvidence)
    ));

    let expiry = Timestamp::new(now.seconds() + 10, now.nanoseconds())?;
    let fresh = OpenRouterChatModelEvidence::from_authenticated_discovery(
        ModelId::new("vendor/model")?,
        route.identity(),
        crate::chat::OpenRouterChatDiscovery {
            endpoints: vec![candidate],
            model_supported_parameters: BTreeSet::new(),
            reasoning: None,
            context_length: 4096,
            image_input: false,
            observed_at: Timestamp::new(now.seconds().saturating_sub(1), now.nanoseconds())?,
            expires_at: expiry,
        },
    )?;
    // The reviewed Chat facts never refuse on the calendar: a decade past
    // their review-due date the route builds, and its authority is
    // bounded by discovery evidence alone.
    let past_review = Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEW_DUE_AT + 10 * 365 * 86_400,
        0,
    )?;
    let future_route = route
        .clone()
        .with_clock(Arc::new(plakous_model::FixedClock::new(past_review)));
    let future_evidence = OpenRouterChatModelEvidence::from_authenticated_discovery(
        ModelId::new("vendor/model")?,
        future_route.identity(),
        crate::chat::OpenRouterChatDiscovery {
            endpoints: fresh.endpoints().to_vec(),
            model_supported_parameters: BTreeSet::new(),
            reasoning: None,
            context_length: 4096,
            image_input: false,
            observed_at: past_review,
            expires_at: Timestamp::new(past_review.seconds() + 60, 0)?,
        },
    )?;
    let future_evidence_expiry = future_evidence.expires_at();
    let future_target =
        OpenRouterChatTarget::gateway_managed(future_route, future_evidence, context.clone())?;
    let future_selected = selected_for(&future_target)?;
    let future_model = Arc::new(OpenRouterChatLanguageModel::new(
        future_target,
        future_selected,
        Client::new(Limits::default())?,
    )?);
    let authority = Arc::clone(&future_model)
        .revalidate_runtime_authority(past_review)
        .await?;
    assert_eq!(authority.valid_until(), Some(future_evidence_expiry));
    assert!(matches!(
        future_model
            .revalidate_runtime_authority(future_evidence_expiry)
            .await,
        Err(plakous_model::RuntimeModelAuthorityError::Stale)
    ));
    let target = OpenRouterChatTarget::gateway_managed(route, fresh, context)?;
    let selected = selected_for(&target)?;
    let model = Arc::new(OpenRouterChatLanguageModel::new(
        target,
        selected,
        Client::new(Limits::default())?,
    )?);
    assert!(matches!(
        model.revalidate_runtime_authority(expiry).await,
        Err(plakous_model::RuntimeModelAuthorityError::Stale)
    ));
    Ok(())
}

struct DiscoveryWirePeer {
    client: Client,
    origin_text: String,
    peer: tokio::task::JoinHandle<io::Result<Vec<String>>>,
}

async fn discovery_wire_peer() -> Result<DiscoveryWirePeer, Box<dyn std::error::Error>> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
    let certificate = cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut server_config = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], private_key)?;
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(certificate.to_vec()))?;
    let client_config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tls_identity = TlsConfigIdentity::from_parts(
        1,
        &[b"openrouter-chat-discovery-test-v1", certificate.as_ref()],
    )?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin_text = format!("https://{}", listener.local_addr()?);
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let responses = [
        br#"{"data":[{"id":"vendor/model-v1:free","canonical_slug":"vendor/model-v1:free","context_length":32768,"reasoning":{"supported_efforts":["high","medium","low"],"default_effort":"medium","default_enabled":true,"supports_max_tokens":false,"mandatory":false},"supported_parameters":["tools","tool_choice","reasoning"]}],"links":{},"total_count":1}"#.to_vec(),
        br#"{"data":{"id":"vendor/model-v1:free","endpoints":[{"tag":"provider/model-v1","provider_name":"Provider","context_length":32768,"max_prompt_tokens":28672,"max_completion_tokens":4096,"supported_parameters":["tools","tool_choice"],"supports_tool_choice":{"none":true,"auto":true,"required":false,"function":false},"supports_voice_cloning":false}]}}"#.to_vec(),
    ];
    let responses = responses
        .into_iter()
        .chain([br#"{"data":[{"id":"vendor/model-v1:free"},{"id":"other/model"}]}"#.to_vec()]);
    let peer = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in responses {
            let (stream, _) = listener.accept().await?;
            let mut stream = acceptor.accept(stream).await?;
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "request head"));
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.write_all(&body).await?;
            stream.shutdown().await?;
            requests.push(String::from_utf8(request).map_err(io::Error::other)?);
        }
        Ok::<_, io::Error>(requests)
    });
    Ok(DiscoveryWirePeer {
        client,
        origin_text,
        peer,
    })
}

#[tokio::test]
async fn authenticated_loopback_discovery_binds_exact_literal_and_paths()
-> Result<(), Box<dyn std::error::Error>> {
    let DiscoveryWirePeer {
        client,
        origin_text,
        peer,
    } = discovery_wire_peer().await?;
    let source = Arc::new(StaticOpenRouterApiKeySource::new(
        OpenRouterApiKey::new("test-catalog-key")?,
        "test-account",
        "test-authority",
    )?);
    let route = OpenRouterChatRoute::new_local_fixture_with_retention(
        source,
        OpenRouterOrigin::new(origin_text)?,
        OpenRouterRetentionPolicy::strict(),
    )?
    .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
        crate::chat::OPENROUTER_CHAT_REVIEWED_AT + 60,
        0,
    )?)));
    let evidence = route
        .discover_chat_model(
            &client,
            "vendor/model-v1:free",
            OpenRouterChatDiscoveryConfig::default(),
        )
        .await?;
    assert_eq!(evidence.model().as_str(), "vendor/model-v1:free");
    assert_eq!(evidence.context_length(), 32_768);
    assert!(evidence.supported_parameters().contains("tools"));
    let endpoint = evidence
        .endpoints()
        .first()
        .ok_or_else(|| io::Error::other("discovery omitted endpoint"))?;
    assert_eq!(endpoint.context_length(), Some(32_768));
    assert_eq!(endpoint.max_prompt_tokens(), Some(28_672));
    assert_eq!(endpoint.max_completion_tokens(), Some(4_096));
    let context = custom_context(&route, 16_384)?;
    let target =
        OpenRouterChatTarget::gateway_managed(route.clone(), evidence.clone(), context.clone())?;
    assert_eq!(
        target.descriptor().integration_class(),
        plakous_model::IntegrationClass::Custom
    );
    let descriptor_context = target
        .descriptor()
        .context_contract()
        .ok_or_else(|| io::Error::other("curated target lacks context"))?;
    assert!(matches!(
        descriptor_context.capacity().evidence_class(),
        plakous_model::ContextEvidenceClass::CustomEnvelope { .. }
    ));
    let selected = selected_for(&target)?;
    let listed = route
        .list_models(&client, OpenRouterChatDiscoveryConfig::default())
        .await?;
    assert_eq!(
        listed.iter().map(String::as_str).collect::<Vec<_>>(),
        ["vendor/model-v1:free", "other/model"]
    );
    let configured = OpenRouterChatLanguageModel::configured_gateway_managed(
        route,
        evidence,
        context,
        selected.clone(),
        client,
    )?;
    assert_eq!(configured.configuration().selected(), &selected);
    assert_eq!(
        configured
            .configuration()
            .effective()
            .descriptor()
            .integration_class(),
        plakous_model::IntegrationClass::Custom
    );
    assert_eq!(
        configured.configuration().selected_identity(),
        selected.identity()
    );
    let requests = peer.await??;
    assert_eq!(requests.len(), 3);
    assert!(requests[2].starts_with("GET /api/v1/models HTTP/1.1\r\n"));
    assert!(requests[0].starts_with("GET /api/v1/models HTTP/1.1\r\n"));
    assert!(
        requests[1].starts_with("GET /api/v1/models/vendor/model-v1%3Afree/endpoints HTTP/1.1\r\n")
    );
    assert!(requests.iter().all(|request| request.lines().any(|line| {
        line.to_ascii_lowercase()
            .starts_with("authorization: bearer ")
    })));
    Ok(())
}
