#![allow(
    clippy::unwrap_used,
    reason = "unit tests fail fast on invalid fixtures"
)]

use super::tests::{
    LOCAL_SUCCESS_SSE, TEST_GRANT_BINDING, inference_gate, local_peer, local_response, manager,
    manager_with_client, private_store,
};
use super::*;
use crate::oauth::CLAUDE_OAUTH_INFERENCE_SCOPE;
use plakous_agent::internal::{
    AgentBuilder, DurableRuntimeOpen, DurableSessionRuntime, RuntimeDurability,
};
use plakous_auth::subscription_limit::{SubscriptionRequestClass, SubscriptionRequestGate};
use plakous_http::Limits;
use plakous_model::*;
use std::time::Duration;
use tokio::time::timeout;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

async fn builder(
    manager: &ClaudeNativeAuthManager,
    root: &std::path::Path,
) -> TestResult<AgentBuilder> {
    let (model, authority, descriptor) = manager
        .resolve_model(
            "claude-sonnet-5",
            Client::new_http1(Limits::default())?,
            SessionLineage::Root,
        )
        .await?;
    configured_builder(model, authority, descriptor, root)
}

pub(super) fn configured_builder(
    model: Arc<dyn LanguageModel>,
    authority: RuntimeModelAuthority,
    descriptor: ResolvedModelDescriptor,
    root: &std::path::Path,
) -> TestResult<AgentBuilder> {
    configured_builder_with_tools(
        model,
        authority,
        descriptor,
        root,
        plakous_tools::ToolRegistryBuilder::default().freeze(),
    )
}

fn configured_builder_with_tools(
    model: Arc<dyn LanguageModel>,
    authority: RuntimeModelAuthority,
    descriptor: ResolvedModelDescriptor,
    root: &std::path::Path,
    tools: plakous_tools::FrozenToolRegistry,
) -> TestResult<AgentBuilder> {
    let revision = ProviderProfileRevisionId::new(CLAUDE_OAUTH_ADAPTER_REVISION)?;
    let profile = ModelAdapterProfileId::new(CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID)?;
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            profile.clone(),
            revision.clone(),
            profile.clone(),
            revision.clone(),
            ProviderAuthorityClassId::new("oauth-subscription")?,
            CredentialSelectionId::new("primary")?,
            descriptor.requested_model().clone(),
        ),
        ProviderBehaviorConfiguration {
            processing_tier: ProcessingTierDisposition::Standard,
            tool_mode: ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Disabled,
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::Omit,
            replay: ReplayDisposition::StrictEncrypted,
            cache: CachePolicy::Disabled,
            tools: tools
                .definitions()
                .iter()
                .cloned()
                .map(|definition| {
                    ConfiguredTool::new(
                        definition,
                        ToolExecutionDisposition::Local,
                        ToolCallerDisposition::DirectOnly,
                    )
                })
                .collect(),
            hosted_tools: vec![],
            tool_choice: ToolChoice::Auto,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::NoRequestBound,
            storage: ProviderStorageDisposition::OmittedRouteContract,
            retention: RetentionDisposition::RouteContract { profile, revision },
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::Ephemeral,
        },
    )?;
    let binding = ResolvedProviderAuthorityBinding::new(
        &descriptor,
        &authority,
        ProviderSemanticEvidence::new(
            1,
            b"plakous-anthropic/claude-oauth-semantic-evidence-v1".to_vec(),
        )?,
    )?;
    let configuration = crate::compile_messages_oauth_configuration(
        &selected,
        descriptor,
        binding,
        crate::ANTHROPIC_API_VERSION,
        crate::claude_oauth_beta_header("claude-sonnet-5"),
    )?;
    let configured = ConfiguredLanguageModel::new(
        &StandardIntegrationAuthority::release_owned(),
        model,
        configuration,
        authority,
    )?;
    Ok(AgentBuilder::new(
        configured,
        tools,
        Arc::new(plakous_tools::WorkspacePolicy::host(root, ".")?),
    ))
}

async fn exact_reopen(different_account: bool) -> TestResult {
    let temp = tempfile::tempdir()?;
    let manager = manager(private_store(&temp));
    manager.persist_test_grant(
        "first-access",
        "first-refresh",
        3600,
        CLAUDE_OAUTH_INFERENCE_SCOPE,
    )?;
    let runtime = DurableSessionRuntime::current(
        Arc::new(plakous_store::MemorySessionStore::new(
            plakous_store::MemoryStoreLimits::default(),
        )?),
        RuntimeDurability::Disabled,
        std::num::NonZeroU64::new(60).unwrap(),
    )?;
    let agent = runtime
        .create("auth-binding-create", builder(&manager, temp.path()).await?)
        .await?;
    let plan = agent.harness_plan().clone();
    let address = DurableSessionRuntime::genesis_address_for("auth-binding-create")?;
    agent.shutdown().await?;
    manager.persist_test_grant_for_account(
        "second-access",
        "second-refresh",
        3600,
        CLAUDE_OAUTH_INFERENCE_SCOPE,
        if different_account {
            "87654321-4321-4321-8321-cba987654321"
        } else {
            TEST_GRANT_BINDING
        },
    )?;
    let result = runtime
        .open(
            "auth-binding-open",
            address,
            builder(&manager, temp.path()).await?.harness_plan(plan),
        )
        .await;
    if different_account {
        let error = result
            .err()
            .ok_or("different account reopened the session")?;
        assert_eq!(error.kind(), plakous_agent::AgentErrorKind::InvalidInput);
        assert!(
            error
                .to_string()
                .contains("resolved harness plan is bound to a different "),
            "{error}"
        );
        assert!(error.binding_change().is_some(), "{error:?}");
        assert!(!error.to_string().contains("provider account changed"));
    } else {
        let DurableRuntimeOpen::Opened(agent) = result? else {
            return Err("reopen was not attached".into());
        };
        agent.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn exact_reopen_accepts_same_account_relogin() -> TestResult {
    exact_reopen(false).await
}

#[tokio::test]
async fn exact_reopen_refuses_different_account_relogin() -> TestResult {
    exact_reopen(true).await
}

struct ReloginTool {
    manager: ClaudeNativeAuthManager,
    different_account: bool,
}

impl plakous_tools::ToolExecutor for ReloginTool {
    fn execute(
        self: Arc<Self>,
        _: plakous_tools::ToolInvocation,
        _: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            self.manager
                .persist_test_grant_for_account(
                    "second-access",
                    "second-refresh",
                    3600,
                    CLAUDE_OAUTH_INFERENCE_SCOPE,
                    if self.different_account {
                        "87654321-4321-4321-8321-cba987654321"
                    } else {
                        TEST_GRANT_BINDING
                    },
                )
                .map_err(|error| {
                    plakous_tools::ToolRuntimeError::new(
                        plakous_tools::ToolRuntimeErrorKind::Execution,
                        error.to_string(),
                    )
                })?;
            plakous_tools::ToolOutcome::success_text("login renewed")
        })
    }
}

fn relogin_registry(
    manager: ClaudeNativeAuthManager,
    different_account: bool,
) -> TestResult<plakous_tools::FrozenToolRegistry> {
    use plakous_tools::*;
    let definition = ToolDefinition::new(
        ToolName::new("renew_login")?,
        "1",
        "Renew the fixture login.",
        ToolInputContract::structured(
            serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false}),
        )?,
        ToolOutputContract::model_content(128, 1_048_576)?,
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let mut tools = ToolRegistryBuilder::default();
    tools.register(ToolRegistration::new(
        RegisteredToolId::new("fixture.login")?,
        ToolExecutorId::new("fixture.login")?,
        ToolExecutorBindingIdentity::new(1, &[b"fixture.login"])?,
        definition,
        Arc::new(ReloginTool {
            manager,
            different_account,
        }),
    ))?;
    Ok(tools.freeze())
}

async fn running_turn(different_account: bool) -> TestResult {
    let temp = tempfile::tempdir()?;
    let tool_sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_before_login\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_fixture\",\"name\":\"renew_login\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let mut replies = vec![local_response(200, "text/event-stream", tool_sse)];
    if !different_account {
        replies.push(local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE));
    }
    let (client, origin, peer) = local_peer(replies).await;
    let gate = inference_gate(2);
    let manager = manager_with_client(private_store(&temp), client.clone())
        .with_subscription_request_gate(gate.clone());
    manager.persist_test_grant(
        "first-access",
        "first-refresh",
        3600,
        CLAUDE_OAUTH_INFERENCE_SCOPE,
    )?;
    let (mut model, authority, descriptor) = manager
        .resolve_oauth_model("claude-sonnet-5", client, SessionLineage::Root)
        .await?;
    Arc::get_mut(&mut model)
        .ok_or("model shared before fixture dial binding")?
        .test_dial_origin = Some(origin);
    let tools = relogin_registry(manager, different_account)?;
    let agent =
        configured_builder_with_tools(model, authority, descriptor, temp.path(), tools)?.build()?;
    let mut turn = agent
        .start(
            plakous_agent::CommandId::new("relogin-turn")?,
            "renew the login then finish",
        )
        .await?
        .into_turn()
        .ok_or("turn not admitted")?;
    let mut events = turn.take_events()?;
    let notices = tokio::spawn(async move {
        let mut renewed = 0;
        while let Ok(event) = events.recv().await {
            if let plakous_agent::AgentEventKind::Model {
                event: Some(model), ..
            } = event.kind()
                && let ModelEvent::Diagnostic(diagnostic) = model.as_ref()
                && diagnostic.message().as_str()
                    == "Claude login renewed; continuing on the same account"
            {
                renewed += 1;
            }
        }
        renewed
    });
    let result = timeout(Duration::from_secs(10), turn.result()).await??;
    match result.outcome() {
        plakous_agent::TurnResultOutcome::Completed(_) if !different_account => {}
        plakous_agent::TurnResultOutcome::Failed(error) if different_account => {
            assert!(error.to_string().contains("account changed"), "{error}");
        }
        _ => return Err(format!("unexpected turn outcome: {:?}", result.outcome()).into()),
    }
    agent.shutdown().await?;
    assert_eq!(notices.await?, usize::from(!different_account));
    let requests = timeout(Duration::from_secs(5), peer).await??;
    assert_eq!(requests.len(), if different_account { 1 } else { 2 });
    assert_eq!(
        gate.snapshot()?
            .class(SubscriptionRequestClass::Inference)
            .dispatched(),
        u32::try_from(requests.len())?
    );
    Ok(())
}

#[tokio::test]
async fn running_turn_accepts_same_account_relogin_at_next_request() -> TestResult {
    running_turn(false).await
}

#[tokio::test]
async fn running_turn_refuses_different_account_relogin_at_next_request() -> TestResult {
    running_turn(true).await
}
