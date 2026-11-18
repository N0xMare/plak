//! An Anthropic Platform key through a credential gateway: a route whose key
//! the gateway holds binds the local key's exact binding (the gateway reports
//! the key's credential digest), and its Messages exchange and paginated
//! `/v1/models` reads reach the upstream byte for byte as they do with the
//! local key.

use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;

use futures_util::StreamExt as _;
use plakous_anthropic::{AnthropicKeySource, AnthropicLanguageModel, ApiKey};
use plakous_auth::api_key::{CredentialProviderTag, StaticApiKeySource};
use plakous_auth::custody::{
    CustodyError, CustodySlot, KeyHeaderForm, KeyInjection, KeySource, LocalKey, LocalKeyCustodian,
    LocalKeyStore,
};
use plakous_http::{Client, Limits, Origin};
use plakous_model::{
    Actor, AttemptId, CachePolicy, CancellationToken, CapabilityRequirement,
    ConfiguredLanguageModel, ContentBlock, ContextAdmissionRequirement, ConversationItem,
    ConversationView, Digest, DigestDomain, EntryId, GenerationLimits, LocalDurabilityDisposition,
    ModelAdapterProfileId, ModelCallContext, ModelPreparationContext, ModelRequest,
    OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, Provenance,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderStorageDisposition,
    ReasoningEffort, ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningPolicy,
    ReasoningSummaryDisposition, ReplayDisposition, RequestMetadata, ResponseStateDisposition,
    RetentionDisposition, RunId, SystemClock, TextBlock, ToolChoice, ToolPresentationMode, TurnId,
    ValidationMode,
};
use uuid::Uuid;

use super::{Respond, TestResult, upstream_with};
use crate::gateway::GatewayCustody;
use crate::gateway::pins::anthropic::{
    ANTHROPIC_API_CREDENTIAL_HEADERS, ANTHROPIC_API_UPSTREAM_PINS,
};
use crate::gateway::server::{InProcessGateway, LocalCustody, new_instance};

const KEY: &str = "synthetic-anthropic-key";
const ORIGIN: &str = "https://api.anthropic.com";
const ACCOUNT: &str = "fixture-account";

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

const SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_gateway_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"relayed\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Messages, and two `/v1/models` pages joined by `after_id`.
fn replies() -> Respond {
    Arc::new(|request: &[u8]| {
        if request.starts_with(b"POST /v1/messages ") {
            // The fixture writes one header line from this text; the second
            // is the request id an Anthropic success must carry.
            return (
                "200 OK",
                "text/event-stream\r\nrequest-id: req_gateway_fixture",
                SSE.as_bytes().to_vec(),
            );
        }
        if request.starts_with(b"GET /v1/models?limit=1000&after_id=claude-sonnet-5 ") {
            return (
                "200 OK",
                "application/json",
                br#"{"data":[{"id":"claude-opus-5","type":"model"}],"has_more":false}"#.to_vec(),
            );
        }
        if request.starts_with(b"GET /v1/models?limit=1000 ") {
            return (
                "200 OK",
                "application/json",
                br#"{"data":[{"id":"claude-sonnet-5","type":"model"}],"has_more":true,"last_id":"claude-sonnet-5"}"#
                    .to_vec(),
            );
        }
        (
            "400 Bad Request",
            "application/json",
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"fixture refusal"}}"#
                .to_vec(),
        )
    })
}

fn behavior() -> TestResult<ProviderBehaviorConfiguration> {
    Ok(ProviderBehaviorConfiguration {
        processing_tier: plakous_model::ProcessingTierDisposition::Standard,
        tool_mode: ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Enabled(ReasoningEffort::Low),
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::StrictEncrypted,
        cache: CachePolicy::Disabled,
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::Enforced(NonZeroU32::new(64).ok_or("bound")?),
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: ModelAdapterProfileId::new(
                plakous_anthropic::ANTHROPIC_API_ADAPTIVE_PROFILE_ID,
            )?,
            revision: ProviderProfileRevisionId::new("messages-local-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    })
}

fn configured(
    source: Arc<AnthropicKeySource>,
    client: Client,
) -> TestResult<ConfiguredLanguageModel> {
    Ok(AnthropicLanguageModel::configured_with_client(
        source,
        ACCOUNT,
        &plakous_anthropic::AnthropicApiModel::bundled(
            plakous_anthropic::CLAUDE_SONNET_5_MODEL_ID,
        )?,
        b"fixture-admitted-facts",
        behavior()?,
        NonZeroU64::new(64_000).ok_or("input bound")?,
        client,
    )?)
}

/// The non-secret binding a host freezes for a configured model.
fn binding(configured: &ConfiguredLanguageModel) -> impl PartialEq + std::fmt::Debug {
    let authority = configured.initial_authority();
    (
        configured.configuration().selected_identity(),
        configured.configuration().effective_identity(),
        authority.account_binding(),
        authority.credential_authority_binding(),
        authority.credential_generation(),
    )
}

/// One Messages round.
async fn dispatch(configured: &ConfiguredLanguageModel) -> TestResult {
    let now = plakous_model::Clock::now(&SystemClock);
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
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Low),
        requirement: CapabilityRequirement::Required,
    })
    .tool_choice(ToolChoice::None)
    .limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?)
    .context_admission_requirement(ContextAdmissionRequirement::AnyBounded)
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
    let mut stream = call
        .start(ModelCallContext::new(
            attempt,
            CancellationToken::new(),
            None,
        ))
        .await?;
    while let Some(event) = stream.next().await {
        event?;
    }
    Ok(())
}

#[tokio::test]
async fn an_anthropic_api_key_binds_and_relays_through_a_gateway_byte_for_byte() -> TestResult {
    let upstream = upstream_with(replies()).await?;
    let production = Origin::parse(&format!("{ORIGIN}/"))?;

    // The local key, its clients dialing the production origin at the fixture.
    let local: Arc<AnthropicKeySource> = Arc::new(StaticApiKeySource::for_key(
        ApiKey::new(KEY)?,
        CredentialProviderTag::AnthropicPlatform,
        ORIGIN,
        None,
        (),
    ));
    let direct = || -> TestResult<Client> {
        Ok(
            Client::new_http1(Limits::default())?.with_test_dial_override(
                production.clone(),
                upstream.origin.clone(),
                upstream.client.clone(),
            ),
        )
    };
    let bound = configured(Arc::clone(&local), direct()?)?;
    dispatch(&bound).await?;
    let listed_direct =
        plakous_anthropic::list_anthropic_api_models_over(local.as_ref(), &direct()?).await?;
    let direct_requests = upstream.take();

    // The same key held by an in-process gateway.
    let injection = |kind: &str| {
        (kind == "anthropic").then_some(KeyInjection {
            provider: CredentialProviderTag::AnthropicPlatform,
            header: "x-api-key",
            form: KeyHeaderForm::Raw,
        })
    };
    let custody = LocalCustody::new(new_instance().ok_or("gateway instance")?).with_kind(
        "anthropic",
        Arc::new(LocalKeyCustodian::new(Arc::new(Keys), injection)),
        ANTHROPIC_API_UPSTREAM_PINS,
    );
    let server = InProcessGateway::server(custody, upstream.client.clone())
        .with_dial_override(ORIGIN, upstream.origin.clone());
    let gateway = GatewayCustody::connect(InProcessGateway::endpoint_for(&server), None, |kind| {
        (kind == "anthropic").then_some(ANTHROPIC_API_CREDENTIAL_HEADERS)
    })?;
    let custodied: Arc<AnthropicKeySource> = Arc::new(gateway.key_source(
        "anthropic",
        CredentialProviderTag::AnthropicPlatform,
        ORIGIN,
        None,
        (),
    )?);
    let relayed = || -> TestResult<Client> {
        Ok(gateway
            .transport()
            .client(Client::new_http1(Limits::default())?))
    };
    custodied.prepare().await?;
    let reopened = configured(Arc::clone(&custodied), relayed()?)?;
    assert_eq!(binding(&reopened), binding(&bound));
    dispatch(&reopened).await?;
    let listed_gateway =
        plakous_anthropic::list_anthropic_api_models_over(custodied.as_ref(), &relayed()?).await?;
    let gateway_requests = upstream.take();

    assert_eq!(listed_gateway, listed_direct);
    assert_eq!(listed_direct.len(), 2);
    // Messages POST, then both model pages, byte for byte with the same
    // header order.
    assert_eq!(direct_requests.len(), 3, "{direct_requests:?}");
    assert_eq!(gateway_requests.len(), direct_requests.len());
    for (direct, relayed) in direct_requests.iter().zip(&gateway_requests) {
        assert_eq!(
            String::from_utf8_lossy(relayed),
            String::from_utf8_lossy(direct)
        );
    }
    assert!(direct_requests[0].starts_with(b"POST /v1/messages "));
    assert!(direct_requests[2].starts_with(b"GET /v1/models?limit=1000&after_id=claude-sonnet-5 "));
    Ok(())
}
