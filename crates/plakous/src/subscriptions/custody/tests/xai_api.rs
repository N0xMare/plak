//! An xAI Platform key through a credential gateway: a route whose key the
//! gateway holds admits the local key's exact generation and binding, and its
//! Responses exchange and `/v1/models` read reach the upstream byte for byte
//! as they do with the local key.

use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;

use futures_util::StreamExt as _;
use plakous_auth::api_key::{CredentialProviderTag, StaticApiKeySource};
use plakous_auth::custody::{
    CustodyError, CustodySlot, KeyHeaderForm, KeyInjection, KeySource, LocalKey, LocalKeyCustodian,
    LocalKeyStore,
};
use plakous_http::{Client, Limits, Origin};
use plakous_model::{
    Actor, AttemptId, CachePolicy, CancellationToken, CapabilityRequirement,
    ConfiguredLanguageModel, ContentBlock, ContextAdmissionRequirement, ConversationItem,
    ConversationView, Digest, DigestDomain, EntryId, FixedClock, GenerationLimits,
    LocalDurabilityDisposition, ModelAdapterProfileId, ModelCallContext, ModelPreparationContext,
    ModelRequest, OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, Provenance,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderStorageDisposition,
    ReasoningEffort, ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningPolicy,
    ReasoningSummaryDisposition, ReplayDisposition, RequestMetadata, ResponseStateDisposition,
    RetentionDisposition, RunId, TextBlock, Timestamp, ToolChoice, ToolPresentationMode, TurnId,
    ValidationMode,
};
use plakous_xai::{ApiKey, XaiPlatformApiKeyAuthority, XaiPlatformKeySource, XaiPlatformModel};
use uuid::Uuid;

use super::{Respond, TestResult, upstream_with};
use crate::gateway::GatewayCustody;
use crate::gateway::pins::xai::{XAI_API_CREDENTIAL_HEADERS, XAI_API_UPSTREAM_PINS};
use crate::gateway::server::{InProcessGateway, LocalCustody, new_instance};

const KEY: &str = "synthetic-xai-platform-key";
const ORIGIN: &str = "https://api.x.ai";
const MODEL: &str = plakous_xai::XAI_PLATFORM_GROK_46_MODEL;

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

fn now() -> TestResult<Timestamp> {
    Ok(Timestamp::new(
        plakous_xai::XAI_PLATFORM_REVIEW_DUE_AT_UNIX_SECONDS - 90 * 86_400 + 60,
        0,
    )?)
}

fn authority() -> TestResult<XaiPlatformApiKeyAuthority> {
    Ok(XaiPlatformApiKeyAuthority::new(
        "fixture-account",
        "fixture-xai-key",
        1,
    )?)
}

/// One completed Responses stream, and the model list.
fn replies() -> Respond {
    Arc::new(|request: &[u8]| {
        if request.starts_with(b"POST /v1/responses ") {
            let message = serde_json::json!({"type":"message","id":"msg_gateway","status":"completed","role":"assistant","content":[{"type":"output_text","text":"relayed"}]});
            let events = [
                serde_json::json!({"type":"response.created","response":{"id":"resp_gateway","object":"response","status":"in_progress","model":MODEL}}),
                serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_gateway","status":"in_progress","role":"assistant","content":[]}}),
                serde_json::json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}),
                serde_json::json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"relayed"}),
                serde_json::json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":"relayed"}}),
                serde_json::json!({"type":"response.output_item.done","output_index":0,"item":message}),
                serde_json::json!({"type":"response.completed","response":{"id":"resp_gateway","object":"response","status":"completed","model":MODEL,"output":[message],"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}}),
            ];
            let mut body = String::new();
            for (index, mut event) in events.into_iter().enumerate() {
                event["sequence_number"] = serde_json::json!(index);
                body.push_str("data: ");
                body.push_str(&event.to_string());
                body.push_str("\n\n");
            }
            return ("200 OK", "text/event-stream", body.into_bytes());
        }
        if request.starts_with(b"GET /v1/models ") {
            return (
                "200 OK",
                "application/json",
                format!(r#"{{"data":[{{"id":"{MODEL}","created":1,"context_length":500000}}]}}"#)
                    .into_bytes(),
            );
        }
        (
            "400 Bad Request",
            "application/json",
            br#"{"error":{"type":"invalid_request_error","message":"fixture refusal"}}"#.to_vec(),
        )
    })
}

fn selected() -> TestResult<plakous_model::SelectedProviderConfiguration> {
    Ok(plakous_xai::xai_platform_selection_for(
        MODEL,
        ProviderBehaviorConfiguration {
            processing_tier: plakous_model::ProcessingTierDisposition::Standard,
            tool_mode: ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Enabled(ReasoningEffort::Low),
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::EncryptedContent,
            replay: ReplayDisposition::StrictEncrypted,
            cache: CachePolicy::Disabled,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::Enforced(NonZeroU32::new(128).ok_or("bound")?),
            storage: ProviderStorageDisposition::DisabledOnWire,
            retention: RetentionDisposition::RouteContract {
                profile: ModelAdapterProfileId::new(plakous_xai::XAI_PLATFORM_GROK_46_PROFILE)?,
                revision: ProviderProfileRevisionId::new(
                    plakous_xai::XAI_PLATFORM_EXECUTION_REVISION,
                )?,
            },
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::DurableRequired,
        },
    )?)
}

async fn configured(
    source: Arc<XaiPlatformKeySource>,
    client: Client,
) -> TestResult<ConfiguredLanguageModel> {
    source.prepare().await?;
    Ok(Arc::new(XaiPlatformModel::with_admitted_facts(
        authority()?,
        source,
        NonZeroU64::new(65_536).ok_or("input bound")?,
        selected()?,
        Arc::new(FixedClock::new(now()?)),
        client,
        b"fixture-admitted-facts",
    )?)
    .configured()
    .await?)
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

/// One Responses round.
async fn dispatch(configured: &ConfiguredLanguageModel) -> TestResult {
    let now = now()?;
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
    .limits(GenerationLimits::new(NonZeroU32::new(128), Vec::new())?)
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
async fn an_xai_api_key_binds_and_relays_through_a_gateway_byte_for_byte() -> TestResult {
    let upstream = upstream_with(replies()).await?;
    let production = Origin::parse(&format!("{ORIGIN}/"))?;

    // The local key, its clients dialing the production origin at the fixture.
    let local: Arc<XaiPlatformKeySource> = Arc::new(StaticApiKeySource::for_key(
        ApiKey::new(KEY)?,
        CredentialProviderTag::XaiPlatform,
        ORIGIN,
        None,
        authority()?,
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
    let bound = configured(Arc::clone(&local), direct()?).await?;
    dispatch(&bound).await?;
    let listed_direct = plakous_xai::list_xai_api_models_over(local.as_ref(), &direct()?).await?;
    let direct_requests = upstream.take();

    // The same key held by an in-process gateway.
    let injection = |kind: &str| {
        (kind == "xai").then_some(KeyInjection {
            provider: CredentialProviderTag::XaiPlatform,
            header: "authorization",
            form: KeyHeaderForm::Bearer,
        })
    };
    let custody = LocalCustody::new(new_instance().ok_or("gateway instance")?).with_kind(
        "xai",
        Arc::new(LocalKeyCustodian::new(Arc::new(Keys), injection)),
        XAI_API_UPSTREAM_PINS,
    );
    let server = InProcessGateway::server(custody, upstream.client.clone())
        .with_dial_override(ORIGIN, upstream.origin.clone());
    let gateway = GatewayCustody::connect(InProcessGateway::endpoint_for(&server), None, |kind| {
        (kind == "xai").then_some(XAI_API_CREDENTIAL_HEADERS)
    })?;
    let custodied: Arc<XaiPlatformKeySource> = Arc::new(gateway.key_source(
        "xai",
        CredentialProviderTag::XaiPlatform,
        ORIGIN,
        None,
        authority()?,
    )?);
    let relayed = || -> TestResult<Client> {
        Ok(gateway
            .transport()
            .client(Client::new_http1(Limits::default())?))
    };
    let reopened = configured(Arc::clone(&custodied), relayed()?).await?;
    assert_eq!(binding(&reopened), binding(&bound));
    dispatch(&reopened).await?;
    let listed_gateway =
        plakous_xai::list_xai_api_models_over(custodied.as_ref(), &relayed()?).await?;
    let gateway_requests = upstream.take();

    assert_eq!(listed_gateway, listed_direct);
    // Responses POST and the model list, byte for byte with the same header
    // order.
    assert_eq!(direct_requests.len(), 2, "{direct_requests:?}");
    assert_eq!(gateway_requests.len(), direct_requests.len());
    for (direct, relayed) in direct_requests.iter().zip(&gateway_requests) {
        assert_eq!(
            String::from_utf8_lossy(relayed),
            String::from_utf8_lossy(direct)
        );
    }
    assert!(direct_requests[0].starts_with(b"POST /v1/responses "));
    Ok(())
}
