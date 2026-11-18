//! OpenRouter Chat through a credential gateway: a route whose key the
//! gateway holds admits the local key's exact generation, and its catalog,
//! endpoint-list and Chat exchanges reach the upstream as pinned rows byte
//! for byte as they do with the local key.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use plakous_auth::api_key::CredentialProviderTag;
use plakous_auth::custody::{
    CustodyError, CustodySlot, KeyHeaderForm, KeyInjection, KeySource, LocalKey, LocalKeyCustodian,
    LocalKeyStore,
};
use plakous_http::{Client, Limits, Origin};
use plakous_model::{
    Actor, AttemptId, CachePolicy, CancellationToken, CapacityCertificate, CapacityRule, Clock,
    ConfiguredLanguageModel, ContentBlock, ContextAdmissionRequirement, ContextContract,
    ContextEvidenceSource, ConversationItem, ConversationView, CountCertificateKind, Digest,
    DigestDomain, EntryId, EvidenceFreshness, FixedClock, GenerationLimits,
    LocalDurabilityDisposition, MeasurementCertificate, ModelAdapterProfileId, ModelCallContext,
    ModelPreparationContext, ModelRequest, OutputBoundDisposition, OutputPolicy,
    ParallelToolDisposition, ProtocolId, Provenance, ProviderBehaviorConfiguration,
    ProviderProfileRevisionId, ProviderStorageDisposition, ReasoningEffortDisposition,
    ReasoningIncludeDisposition, ReasoningSummaryDisposition, ReplayDisposition, RequestMetadata,
    ResponseStateDisposition, RetentionDisposition, RunId, RuntimeModelAuthority, TextBlock,
    Timestamp, ToolChoice, ToolPresentationMode, TurnId, ValidationMode,
};
use plakous_openrouter::{
    ApiKey, CHAT_COMPLETIONS_FAMILY_ID, OpenRouterChatDiscoveryConfig, OpenRouterChatLanguageModel,
    OpenRouterChatRoute, OpenRouterCredentialBinding, OpenRouterKeySource,
    OpenRouterRetentionPolicy,
};
use uuid::Uuid;

use super::{Respond, TestResult, upstream_with};
use crate::gateway::GatewayCustody;
use crate::gateway::pins::openrouter::{OPENROUTER_CREDENTIAL_HEADERS, OPENROUTER_UPSTREAM_PINS};
use crate::gateway::server::{InProcessGateway, LocalCustody, new_instance};

const KEY: &str = "synthetic-openrouter-key";
const ORIGIN: &str = "https://openrouter.ai";
const MODEL: &str = "vendor/model-v1:free";
const ACCOUNT: &str = "fixture-account";
const AUTHORITY: &str = "fixture-authority";

struct Keys;

impl LocalKeyStore for Keys {
    fn key(&self, _slot: &CustodySlot) -> Result<Option<LocalKey>, CustodyError> {
        Ok(Some(LocalKey {
            key: ApiKey::new(KEY).map_err(|_| CustodyError::Store)?,
            source: KeySource::File,
            env_var: None,
        }))
    }
}

fn review_clock() -> TestResult<FixedClock> {
    Ok(FixedClock::new(Timestamp::new(
        plakous_openrouter::OPENROUTER_CHAT_REVIEW_DUE_AT - 90 * 86_400 + 60,
        0,
    )?))
}

/// The catalog, the model's endpoint list and one Chat stream.
fn openrouter_replies() -> Respond {
    Arc::new(|request: &[u8]| {
        if request.starts_with(b"GET /api/v1/models/") {
            let body = format!(
                r#"{{"data":{{"id":"{MODEL}","endpoints":[{{"tag":"provider/model-v1","provider_name":"Provider","supported_parameters":["tools","tool_choice"]}}]}}}}"#
            );
            return ("200 OK", "application/json", body.into_bytes());
        }
        if request.starts_with(b"GET /api/v1/models ") {
            let body = format!(
                r#"{{"data":[{{"id":"{MODEL}","canonical_slug":"{MODEL}","context_length":4096,"supported_parameters":["tools","tool_choice"]}}],"links":{{}},"total_count":1}}"#
            );
            return ("200 OK", "application/json", body.into_bytes());
        }
        if request.starts_with(b"POST /api/v1/chat/completions ") {
            let chunk = |choices: &str| {
                format!(
                    "data: {{\"id\":\"chat-exact\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"{MODEL}\",\"choices\":{choices}}}\n\n"
                )
            };
            let body = [
                chunk(
                    r#"[{"index":0,"delta":{"role":"assistant","content":"chat"},"finish_reason":null}]"#,
                ),
                chunk(r#"[{"index":0,"delta":{},"finish_reason":"stop"}]"#),
                format!(
                    "data: {{\"id\":\"chat-exact\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"{MODEL}\",\"choices\":[],\"usage\":{{\"prompt_tokens\":4,\"completion_tokens\":1,\"total_tokens\":5}}}}\n\n"
                ),
                "data: [DONE]\n\n".to_owned(),
            ]
            .concat();
            return ("200 OK", "text/event-stream", body.into_bytes());
        }
        (
            "400 Bad Request",
            "application/json",
            br#"{"error":{"message":"fixture refusal"}}"#.to_vec(),
        )
    })
}

fn behavior() -> TestResult<ProviderBehaviorConfiguration> {
    Ok(ProviderBehaviorConfiguration {
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
        output_bound: OutputBoundDisposition::Enforced(NonZeroU32::new(64).ok_or("bound")?),
        storage: ProviderStorageDisposition::DisabledOnWire,
        retention: RetentionDisposition::RouteContract {
            profile: ModelAdapterProfileId::new("openrouter.chat.dynamic-v1")?,
            revision: ProviderProfileRevisionId::new("openrouter-chat-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    })
}

fn context(route: &OpenRouterChatRoute) -> TestResult<ContextContract> {
    let authority = RuntimeModelAuthority::authority_class_for(
        route.identity().account_binding(),
        route.identity().credential_authority_binding(),
    )?;
    let declaration = Digest::sha256(
        DigestDomain::ProviderSemanticEvidence,
        1,
        &[
            b"test/openrouter-gateway-context-v1",
            route.identity().fingerprint().as_bytes(),
        ],
    )?;
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, None)?;
    let capacity = CapacityCertificate::custom_envelope(
        "test-openrouter-gateway-capacity",
        "v1",
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(4_096),
            maximum_projected_input: 2_048,
            auto_compact_input: None,
        },
        authority,
        freshness,
        declaration,
        "v1",
    )?;
    let measurement = MeasurementCertificate::new(
        "test-openrouter-gateway-measurement",
        "v1",
        "complete-lowered-request-body-utf8-byte-upper-bound",
        "v1",
        ContextEvidenceSource::CallerDeclaration,
        CountCertificateKind::CustomEnvelopeUpperBound { declaration },
        Some(authority),
        freshness,
    )?;
    Ok(ContextContract::new(
        ProtocolId::new(CHAT_COMPLETIONS_FAMILY_ID)?,
        "openrouter-chat-custom-accounting-v1",
        "openrouter-chat-canonical-v1",
        "test-openrouter-output-reservation-v1",
        capacity,
        vec![measurement],
    )?)
}

/// Discovers the exact model with `source` over `client` and seals the
/// Chat lane.
async fn configured(
    source: Arc<OpenRouterKeySource>,
    client: Client,
) -> TestResult<ConfiguredLanguageModel> {
    source.prepare().await?;
    let route =
        OpenRouterChatRoute::new_with_retention(source, OpenRouterRetentionPolicy::strict())?
            .with_clock(Arc::new(review_clock()?));
    let evidence = route
        .discover_chat_model(
            &client,
            MODEL,
            OpenRouterChatDiscoveryConfig {
                operation_timeout: Duration::from_secs(5),
                maximum_body_bytes: 64 * 1_024,
                evidence_ttl: Duration::from_secs(60),
            },
        )
        .await?;
    let context = context(&route)?;
    Ok(
        OpenRouterChatLanguageModel::configured_gateway_managed_behavior(
            route,
            evidence,
            context,
            behavior()?,
            client,
        )?,
    )
}

/// One Chat round.
async fn dispatch(configured: &ConfiguredLanguageModel) -> TestResult {
    let now = review_clock()?.now();
    let attempt = AttemptId::try_from(Uuid::from_u128(3))?;
    let request = ModelRequest::builder(
        configured.configuration().selected_identity(),
        configured.configuration().effective_identity(),
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(6))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("fixture prompt", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1))?,
            TurnId::try_from(Uuid::from_u128(2))?,
            attempt,
            now,
        ),
    )
    .tool_choice(ToolChoice::None)
    .limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?)
    .resolved_harness_plan_identity(Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"fixture-harness-plan"],
    )?)
    .build()?;
    let model = configured.model();
    let plan = configured
        .registered()
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let authority = model.clone().revalidate_runtime_authority(now).await?;
    let call = model.prepare_call(plan, ModelPreparationContext::new(authority, now))?;
    // The wire bytes are the evidence; the fixture's stream need not satisfy
    // the lane's terminal accounting.
    if let Ok(mut stream) = call
        .start(ModelCallContext::new(
            attempt,
            CancellationToken::new(),
            None,
        ))
        .await
    {
        while stream.next().await.is_some() {}
    }
    Ok(())
}

#[tokio::test]
async fn an_openrouter_chat_lane_relays_through_a_gateway_byte_for_byte() -> TestResult {
    let upstream = upstream_with(openrouter_replies()).await?;

    // The local key, its client dialing the production origin at the fixture.
    let local: Arc<OpenRouterKeySource> =
        Arc::new(plakous_openrouter::static_openrouter_api_key_source(
            ApiKey::new(KEY)?,
            ACCOUNT,
            AUTHORITY,
        )?);
    let direct = Client::new_http1(Limits::default())?.with_test_dial_override(
        Origin::parse(&format!("{ORIGIN}/"))?,
        upstream.origin.clone(),
        upstream.client.clone(),
    );
    dispatch(&configured(local, direct).await?).await?;
    let direct_requests = upstream.take();

    // The same key held by an in-process gateway.
    let injection = |kind: &str| {
        (kind == "openrouter").then_some(KeyInjection {
            provider: CredentialProviderTag::OpenRouterPlatform,
            header: "authorization",
            form: KeyHeaderForm::Bearer,
        })
    };
    let custody = LocalCustody::new(new_instance().ok_or("gateway instance")?).with_kind(
        "openrouter",
        Arc::new(LocalKeyCustodian::new(Arc::new(Keys), injection)),
        OPENROUTER_UPSTREAM_PINS,
    );
    let server = InProcessGateway::server(custody, upstream.client.clone())
        .with_dial_override(ORIGIN, upstream.origin.clone());
    let gateway = GatewayCustody::connect(InProcessGateway::endpoint_for(&server), None, |kind| {
        (kind == "openrouter").then_some(OPENROUTER_CREDENTIAL_HEADERS)
    })?;
    let custodied: Arc<OpenRouterKeySource> = Arc::new(gateway.key_source(
        "openrouter",
        CredentialProviderTag::OpenRouterPlatform,
        ORIGIN,
        None,
        OpenRouterCredentialBinding::platform(ACCOUNT, AUTHORITY)?,
    )?);
    let relayed = gateway
        .transport()
        .client(Client::new_http1(Limits::default())?);
    dispatch(&configured(custodied, relayed).await?).await?;
    let gateway_requests = upstream.take();

    // Catalog GET, endpoint-list GET and Chat POST, byte for byte with the
    // same header order.
    assert_eq!(direct_requests.len(), 3, "{direct_requests:?}");
    assert_eq!(gateway_requests.len(), direct_requests.len());
    for (direct, relayed) in direct_requests.iter().zip(&gateway_requests) {
        assert_eq!(
            String::from_utf8_lossy(relayed),
            String::from_utf8_lossy(direct)
        );
    }
    assert!(
        direct_requests[2].starts_with(b"POST /api/v1/chat/completions "),
        "{}",
        String::from_utf8_lossy(&direct_requests[2])
    );
    Ok(())
}
