//! Thin Anthropic policy adapter over the Messages request kernel.

use std::fmt;

use plakous_messages::{
    LoweredMessagesRequest, MessagesRequestError, MessagesRequestErrorKind, MessagesRequestPolicy,
    MessagesStreamDialectHooks, MessagesStreamExtensions, MessagesThinkingDisplay,
    PreparedMessagesStreamBinding,
};
use plakous_model::{
    ActiveFamilyArtifact, Digest, ModelId, ReasoningPolicy, SessionId, ToolChoice,
    ToolExecutionOwner, ValidatedRequestPlan,
};

use crate::{
    AnthropicError, AnthropicErrorKind, AnthropicErrorPhase, AnthropicMessagesTargetBinding,
    AnthropicPlatformRoute, ResolvedAnthropicMessages, ThinkingSupport,
};

pub use plakous_messages::{REDACTED_REPLAY_KIND, SIGNATURE_REPLAY_KIND, ToolOwnership};

/// Exact immutable target and plan identity for one prepared operation.
#[derive(Clone, Eq, PartialEq)]
pub struct AnthropicMessagesOperationBinding {
    target: AnthropicMessagesTargetBinding,
    plan_digest: Digest,
    operation_digest: Digest,
}

impl AnthropicMessagesOperationBinding {
    const fn new(
        target: AnthropicMessagesTargetBinding,
        plan_digest: Digest,
        operation_digest: Digest,
    ) -> Self {
        Self {
            target,
            plan_digest,
            operation_digest,
        }
    }
    /// Returns exact Anthropic route, account, model, dialect, and descriptor authority.
    pub const fn target(&self) -> &AnthropicMessagesTargetBinding {
        &self.target
    }
    /// Returns the validated canonical plan identity.
    pub const fn plan_digest(&self) -> Digest {
        self.plan_digest
    }
    /// Returns the credential-free family operation identity.
    pub const fn operation_digest(&self) -> Digest {
        self.operation_digest
    }
}

impl fmt::Debug for AnthropicMessagesOperationBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicMessagesOperationBinding")
            .field("target", &self.target)
            .field("plan_digest", &self.plan_digest)
            .field("operation_digest", &self.operation_digest)
            .finish()
    }
}

/// A serialized, credential-free Anthropic Messages operation.
#[derive(Clone, PartialEq)]
pub struct PreparedMessagesRequest {
    lowered: LoweredMessagesRequest,
    binding: AnthropicMessagesOperationBinding,
    request_policy: MessagesRequestPolicy,
    plan: ValidatedRequestPlan,
    oauth_session_id: Option<SessionId>,
}

impl fmt::Debug for PreparedMessagesRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedMessagesRequest")
            .field("body", &"<redacted>")
            .field("binding", &self.binding)
            .field("tool_ownership", &self.lowered.tool_ownership())
            .finish_non_exhaustive()
    }
}

impl PreparedMessagesRequest {
    pub(crate) const fn oauth_session_id(&self) -> Option<SessionId> {
        self.oauth_session_id
    }

    pub(crate) fn turn_rooted(&self) -> bool {
        matches!(
            self.plan.request().metadata().root(),
            plakous_model::ModelOperationRoot::Turn(_)
        )
    }

    /// Returns deterministic request JSON bytes.
    pub fn body(&self) -> &[u8] {
        self.lowered.body()
    }
    /// Returns the Messages `max_tokens` already written into [`Self::body`].
    pub const fn max_tokens(&self) -> u32 {
        self.lowered.max_tokens()
    }
    /// Returns the exact validated plan digest.
    pub const fn plan_digest(&self) -> Digest {
        self.binding.plan_digest()
    }
    /// Returns the exact credential-free operation digest.
    pub const fn operation_digest(&self) -> Digest {
        self.binding.operation_digest()
    }
    /// Returns bounded non-secret diagnostics for native replay stripped during lowering.
    pub fn replay_diagnostics(&self) -> &[String] {
        self.lowered.replay_diagnostics()
    }
    /// Returns typed notices for completed thinking omitted across configuration transitions.
    pub fn historical_projection_diagnostics(&self) -> &[plakous_model::ModelDiagnostic] {
        self.lowered.historical_projection_diagnostics()
    }
    /// Returns the descriptor binding that accepted the plan.
    pub const fn binding_digest(&self) -> Digest {
        self.binding.target().descriptor_binding()
    }
    /// Returns complete immutable operation authority.
    pub const fn operation_binding(&self) -> &AnthropicMessagesOperationBinding {
        &self.binding
    }
    /// Returns exact immutable Anthropic target authority.
    pub const fn target_binding(&self) -> &AnthropicMessagesTargetBinding {
        self.binding.target()
    }
    /// Revalidates the secret-bearing route immediately before dispatch.
    pub fn validate_route(&self, route: &AnthropicPlatformRoute) -> Result<(), AnthropicError> {
        self.target_binding().validate_route(route)
    }
    /// Returns the immutable route identity.
    pub const fn route_id(&self) -> &plakous_model::RouteId {
        self.binding.target().route_id()
    }
    /// Returns the requested remote model.
    pub const fn model(&self) -> &ModelId {
        self.binding.target().model().remote_id()
    }
    /// Returns the exact provider identity for replay validation.
    pub fn replay_model(&self) -> &ModelId {
        self.binding.target().replay_model()
    }
    /// Returns the frozen harness profile.
    pub const fn profile_id(&self) -> &plakous_model::ModelAdapterProfileId {
        self.binding.target().profile()
    }
    /// Returns the exact API-version/beta dialect.
    pub const fn dialect(&self) -> &crate::AnthropicMessagesDialect {
        self.binding.target().dialect()
    }
    /// Returns exact tool ownership used by stream projection.
    pub const fn tool_ownership(&self) -> &ToolOwnership {
        self.lowered.tool_ownership()
    }
    /// Returns what replaying the exact lowered body can cost.
    pub const fn replay_safety(&self) -> plakous_messages::ReplaySafety {
        self.lowered.replay_safety()
    }

    pub(crate) fn stream_binding(&self) -> Result<PreparedMessagesStreamBinding, AnthropicError> {
        let dialect = MessagesStreamDialectHooks::new(
            crate::dialect::ANTHROPIC_CODE_EXECUTION_CALLER,
            plakous_model::NamespaceId::new("anthropic.programmatic_tool").map_err(|error| {
                AnthropicError::new(
                    AnthropicErrorKind::InvalidConfiguration,
                    AnthropicErrorPhase::Prepare,
                    error.to_string(),
                )
            })?,
        );
        let dialect = if self.dialect().is_claude_oauth() {
            dialect.with_extensions(MessagesStreamExtensions::ClaudeOauthV1)
        } else {
            dialect
        };
        let binding = PreparedMessagesStreamBinding::new(
            self.request_policy.clone(),
            self.plan_digest(),
            self.tool_ownership().clone(),
            dialect,
        )
        .with_artifact_production(self.plan.clone(), self.operation_digest());
        Ok(self.lowered.bind_active_artifact_to_stream(binding))
    }
}

/// Thin exact-target adapter over neutral Messages lowering.
#[derive(Clone)]
pub struct MessagesRequestTranslator {
    oauth_identity: Option<crate::ClaudeOAuthRequestIdentity>,
    resolved: ResolvedAnthropicMessages,
    session_id: Option<SessionId>,
    strict_thinking_replay: bool,
    completed_history_prefix: Option<plakous_model::ConversationPrefixIdentity>,
    previous_response_id: Option<String>,
    turn_position: Option<plakous_model::TurnPosition>,
}

impl fmt::Debug for MessagesRequestTranslator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessagesRequestTranslator")
            .field("resolved", &self.resolved)
            .field("oauth_identity", &self.oauth_identity)
            .field("has_session_id", &self.session_id.is_some())
            .field("strict_thinking_replay", &self.strict_thinking_replay)
            .field("completed_history_prefix", &self.completed_history_prefix)
            .field(
                "has_previous_response_id",
                &self.previous_response_id.is_some(),
            )
            .field("turn_position", &self.turn_position)
            .finish()
    }
}

impl MessagesRequestTranslator {
    /// Freezes one exact Anthropic target.
    pub const fn new(resolved: ResolvedAnthropicMessages) -> Self {
        Self {
            resolved,
            session_id: None,
            oauth_identity: None,
            strict_thinking_replay: false,
            completed_history_prefix: None,
            previous_response_id: None,
            turn_position: None,
        }
    }

    /// Supplies runtime-proven continuation facts for Claude OAuth attribution:
    /// the previous provider response id and the root turn's session position.
    #[must_use]
    pub fn with_continuation(
        mut self,
        previous_response_id: Option<&str>,
        turn_position: Option<plakous_model::TurnPosition>,
    ) -> Self {
        self.previous_response_id = previous_response_id.map(str::to_owned);
        self.turn_position = turn_position;
        self
    }

    pub(crate) const fn require_exact_thinking_replay(mut self) -> Self {
        self.strict_thinking_replay = true;
        self
    }

    pub(crate) const fn with_completed_history_prefix(
        mut self,
        prefix: Option<plakous_model::ConversationPrefixIdentity>,
    ) -> Self {
        self.completed_history_prefix = prefix;
        self
    }

    /// Supplies the runtime-owned logical session identity for Claude OAuth.
    ///
    /// OAuth lowering rejects an absent identity. API-key requests do not use it.
    /// This is the persisted session UUID, not a branch cache-affinity digest.
    #[must_use]
    pub const fn with_session_id(mut self, session_id: Option<SessionId>) -> Self {
        self.session_id = session_id;
        self
    }

    /// Supplies authenticated account and independently persisted client identity.
    #[must_use]
    pub fn with_oauth_identity(mut self, identity: crate::ClaudeOAuthRequestIdentity) -> Self {
        self.oauth_identity = Some(identity);
        self
    }

    /// Validates Anthropic policy then delegates grammar and ordering exactly once.
    pub fn translate(
        &self,
        plan: &ValidatedRequestPlan,
        ownership: ToolOwnership,
    ) -> Result<PreparedMessagesRequest, AnthropicError> {
        self.translate_with_provenance(plan, ownership, None, None)
    }

    /// Validates and consumes one active Messages artifact during exact lowering.
    pub fn translate_with_provenance(
        &self,
        plan: &ValidatedRequestPlan,
        ownership: ToolOwnership,
        active_artifact: Option<&ActiveFamilyArtifact>,
        previous_response_metadata: Option<&plakous_model::ModelResponseMetadata>,
    ) -> Result<PreparedMessagesRequest, AnthropicError> {
        self.resolved.validate_plan(plan)?;
        let target = AnthropicMessagesTargetBinding::from_resolved(&self.resolved)?;
        let policy = policy_for(
            &target,
            plan,
            OAuthContinuation {
                previous: previous_response_metadata,
                previous_response_id: self.previous_response_id.as_deref(),
                turn_position: self.turn_position,
            },
            self.session_id,
            self.oauth_identity.as_ref(),
        )?
        .with_strict_thinking_replay(self.strict_thinking_replay)
        .with_completed_history_prefix(self.completed_history_prefix);
        let lowered = plakous_messages::lower_request_with_active_artifact(
            plan,
            &policy,
            ownership,
            active_artifact,
        )
        .map_err(map_request_error)?;
        let binding = AnthropicMessagesOperationBinding::new(
            target,
            plan.digest(),
            lowered.operation_digest(),
        );
        Ok(PreparedMessagesRequest {
            lowered,
            binding,
            request_policy: policy,
            plan: plan.clone(),
            oauth_session_id: self
                .resolved
                .dialect()
                .is_claude_oauth()
                .then_some(self.session_id)
                .flatten(),
        })
    }
}

/// Runtime-proven facts about the request's place in its session.
struct OAuthContinuation<'a> {
    previous: Option<&'a plakous_model::ModelResponseMetadata>,
    previous_response_id: Option<&'a str>,
    turn_position: Option<plakous_model::TurnPosition>,
}

/// A provider message id native would name in `diagnostics`; anything else is `null`.
fn native_message_id(value: &str) -> bool {
    value.strip_prefix("msg_").is_some_and(|suffix| {
        (1..=128).contains(&suffix.len())
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    })
}

fn policy_for(
    target: &AnthropicMessagesTargetBinding,
    plan: &ValidatedRequestPlan,
    continuation: OAuthContinuation<'_>,
    session_id: Option<SessionId>,
    oauth_identity: Option<&crate::ClaudeOAuthRequestIdentity>,
) -> Result<MessagesRequestPolicy, AnthropicError> {
    if target.dialect().is_claude_oauth()
        && matches!(
            target.model().remote_id().as_str(),
            "claude-opus-5-5" | "claude-sonnet-5-5" | "claude-haiku-5-5"
        )
        && matches!(plan.request().reasoning(), ReasoningPolicy::Disabled)
    {
        return Err(AnthropicError::new(
            AnthropicErrorKind::InvalidRequest,
            AnthropicErrorPhase::Prepare,
            "Claude 5.5 requires adaptive thinking",
        ));
    }
    if target.dialect().is_claude_oauth()
        && target.model().remote_id().as_str() == "claude-sonnet-5-5"
        && matches!(
            plan.request().tool_choice(),
            ToolChoice::Required | ToolChoice::Specific(_)
        )
    {
        return Err(AnthropicError::new(
            AnthropicErrorKind::InvalidRequest,
            AnthropicErrorPhase::Prepare,
            "Claude Sonnet 5.5 OAuth does not admit forced tool choice",
        ));
    }
    let minimum = match target.model().thinking() {
        ThinkingSupport::Disabled | ThinkingSupport::Adaptive => None,
        ThinkingSupport::Manual {
            minimum_budget_tokens,
        } => Some(minimum_budget_tokens),
    };
    let account_binding = target.replay_account_binding().ok_or_else(|| {
        AnthropicError::new(
            AnthropicErrorKind::InvalidConfiguration,
            AnthropicErrorPhase::Prepare,
            "Anthropic replay authority requires a durable non-secret account binding",
        )
    })?;
    let mut policy = MessagesRequestPolicy::new(
        target.protocol().clone(),
        target.model().remote_id().clone(),
        target.expected_reported_model().cloned(),
        target.descriptor_binding(),
        target.semantic_descriptor_binding(),
        account_binding,
        plan.request().selected_provider_configuration_id(),
        plan.request().effective_provider_configuration_id(),
        target.model().maximum_output_tokens(),
        minimum,
        target.dialect().api_version(),
        target.dialect().beta_header().unwrap_or_default(),
    );
    if target.dialect().is_claude_oauth() {
        // Claude Code sends the clear-thinking edit only while thinking is
        // not disabled (`thinking.type !== "disabled"`).
        if !matches!(plan.request().reasoning(), ReasoningPolicy::Disabled) {
            policy = policy.with_claude_oauth_context_management();
        }
        if let Some(cap) =
            crate::claude_oauth_request_max_tokens(target.model().remote_id().as_str())
        {
            policy = policy.with_request_output_cap(cap);
        }
        let session_id = session_id.ok_or_else(|| {
            AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "Claude OAuth requires a runtime logical session identity",
            )
        })?;
        let identity = oauth_identity.ok_or_else(|| {
            AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "Claude OAuth lowering requires authenticated account and installation identity",
            )
        })?;
        let metadata = serde_json::json!({
            "device_id": identity.device_id().as_str(),
            "account_uuid": identity.account_uuid().as_str(),
            "session_id": session_id.to_string(),
        })
        .to_string();
        policy = policy
            .with_system_prefix(crate::claude_oauth_billing_header(
                target.session_lineage(),
                plan,
                continuation.previous,
                continuation.turn_position,
            ))
            .with_claude_oauth_previous_message_id(
                continuation
                    .previous_response_id
                    .filter(|id| native_message_id(id))
                    .map(str::to_owned),
            )
            .with_system_prefix(crate::CLAUDE_OAUTH_IDENTITY)
            .with_serialized_body_transform(crate::attest_claude_oauth_cch)
            .with_metadata_user_id(metadata)
            .with_prompt_cache(true)
            .with_top_level_prompt_cache(false)
            .with_prompt_cache_ttl(Some("1h"))
            .with_thinking_display(MessagesThinkingDisplay::Updates)
            // An explicit disabled-reasoning request with ToolChoice::None
            // emits `thinking: disabled`; ordinary OAuth turns omit it.
            // Semantic compaction preserves its source reasoning policy.
            .with_omitted_disabled_thinking(!matches!(
                plan.request().tool_choice(),
                ToolChoice::None
            ));
    }
    if plan
        .request()
        .hosted_tools()
        .contains(&plakous_model::HostedTool::ProgrammaticComposition)
    {
        if target.profile().as_str() != crate::CLAUDE_SONNET_5_HOSTED_PTC_PROFILE_ID {
            return Err(AnthropicError::new(
                AnthropicErrorKind::InvalidRequest,
                AnthropicErrorPhase::Prepare,
                "Anthropic hosted programmatic tools require the exact Sonnet 5 hosted profile",
            ));
        }
        policy = policy.with_programmatic_tool_calling(
            vec![
                crate::dialect::ANTHROPIC_CODE_EXECUTION_CALLER.to_owned(),
                crate::dialect::ANTHROPIC_CODE_EXECUTION_CALLER_20260521.to_owned(),
            ],
            crate::dialect::ANTHROPIC_CODE_EXECUTION_TOOL,
            plakous_model::NamespaceId::new("anthropic.programmatic_tool").map_err(|error| {
                AnthropicError::new(
                    AnthropicErrorKind::InvalidConfiguration,
                    AnthropicErrorPhase::Prepare,
                    error.to_string(),
                )
            })?,
        );
    }
    Ok(policy)
}

fn map_request_error(error: MessagesRequestError) -> AnthropicError {
    let kind = match error.kind() {
        MessagesRequestErrorKind::UnsupportedSemantic => AnthropicErrorKind::UnsupportedSemantic,
        MessagesRequestErrorKind::InvalidRequest => AnthropicErrorKind::InvalidRequest,
        MessagesRequestErrorKind::ProtectedHistory => AnthropicErrorKind::ProtectedHistory,
    };
    AnthropicError::new(kind, AnthropicErrorPhase::Prepare, error.message())
}

#[allow(
    dead_code,
    reason = "type assertion keeps the family ownership value neutral"
)]
fn _ownership_is_neutral(owner: ToolExecutionOwner) -> ToolExecutionOwner {
    owner
}
