//! Closed SuperGrok facts consumed by the shared Responses-family host.
//!
//! This module deliberately contains no request, stream, projection, attempt,
//! or client state machine. It records only the subscription route's admitted
//! construction data and the small wire deltas that a shared host must apply.

#[cfg(feature = "native-auth")]
use plakous_model::{
    CompiledProviderConfiguration, ReasoningEffortDisposition, ResolvedProviderAuthorityBinding,
    RuntimeModelAuthority, SelectedProviderConfiguration, ToolPresentationMode,
};
use plakous_model::{ModelFamilyId, ModelId, ProtocolId, ReasoningEffort, ReplayKindId};
use plakous_responses::{LifecycleDialectPolicy, ResponsesDialectProfile};
#[cfg(feature = "native-auth")]
use plakous_responses_dialect::{
    ResponsesProviderConfigurationError, compile_provider_configuration,
};
use plakous_responses_dialect::{
    ResponsesProviderErrorClass, ResponsesReplayKind, ResponsesReplayProjection,
    ResponsesRequestPolicy, ResponsesStreamDialectData, ResponsesUsagePolicy,
};

use crate::{XAI_SUBSCRIPTION_RESEARCH_BASELINE, XaiDialect, XaiRouteKind};

/// Exact official Grok OAuth subscription base URL.
pub const SUPERGROK_SUBSCRIPTION_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
/// Exact SuperGrok subscription proxy origin. OAuth has a distinct pinned OIDC origin.
pub const SUPERGROK_RESPONSES_ORIGIN: &str = "https://cli-chat-proxy.grok.com";
/// Exact authenticated subscription catalog resource path.
pub const SUPERGROK_MODELS_PATH: &str = "/v1/models";
/// Exact SuperGrok Responses resource path.
pub const SUPERGROK_RESPONSES_PATH: &str = "/v1/responses";
/// Stable request/adapter revision retained by admission and recovery facts.
pub const XAI_RESPONSES_ADAPTER_REVISION: &str = "plakous-xai-responses-adapter-v7";
/// Exact reviewed route/model capability-table revision.
pub const SUPERGROK_FEATURE_REVISION: &str = "plak-xai-subscription-feature-matrix-v8";
/// Closed shared-host behavior revision for this dialect table.
pub const SUPERGROK_RESPONSES_DIALECT_REVISION: u32 = 5;
/// Exact provider-owned tool presentation admitted for SuperGrok inference.
#[cfg(feature = "native-auth")]
pub const SUPERGROK_TOOL_PRESENTATION_MODE: ToolPresentationMode = ToolPresentationMode::Direct;
/// Native completed reasoning/output-item replay envelope.
pub const SUPERGROK_OUTPUT_ITEM_REPLAY_KIND: &str = "xai.output_item";
/// Revision of [`SUPERGROK_OUTPUT_ITEM_REPLAY_KIND`].
pub const SUPERGROK_OUTPUT_ITEM_REPLAY_VERSION: u32 = 2;
/// Native future-output replay envelope retained by the current projector.
pub const SUPERGROK_OPAQUE_OUTPUT_REPLAY_KIND: &str = "xai.opaque_output";
/// Revision of [`SUPERGROK_OPAQUE_OUTPUT_REPLAY_KIND`].
pub const SUPERGROK_OPAQUE_OUTPUT_REPLAY_VERSION: u32 = 1;

/// How one provider event is handled by the SuperGrok dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuperGrokEventDelta {
    /// A currently typed, non-terminal xAI Responses event.
    Typed,
    /// A typed response aggregate terminal.
    Terminal,
    /// A terminal provider-error envelope.
    ProviderError,
    /// A future event retained opaquely without a support claim.
    PreserveOpaque,
}

/// One closed, credential-free SuperGrok Responses dialect value.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SuperGrokResponsesDialect;

impl SuperGrokResponsesDialect {
    /// Creates the built-in subscription dialect facts.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Exact subscription dialect and protocol revision.
    #[must_use]
    pub const fn dialect(self) -> XaiDialect {
        XaiDialect::GrokSubscriptionResponses2026_07
    }

    /// Exact subscription route family; never xAI Platform.
    #[must_use]
    pub const fn route(self) -> XaiRouteKind {
        XaiRouteKind::GrokSubscription
    }

    /// Stable protocol identity bound into canonical replay.
    #[must_use]
    pub const fn protocol_id(self) -> &'static str {
        self.dialect().protocol_id()
    }

    /// Exact inference origin.
    #[must_use]
    pub const fn origin(self) -> &'static str {
        SUPERGROK_RESPONSES_ORIGIN
    }

    /// Exact inference resource path.
    #[must_use]
    pub const fn responses_path(self) -> &'static str {
        SUPERGROK_RESPONSES_PATH
    }

    /// Existing adapter revision that manifests must continue to bind during cutover.
    #[must_use]
    pub const fn adapter_revision(self) -> &'static str {
        XAI_RESPONSES_ADAPTER_REVISION
    }

    /// Existing subscription feature-matrix revision.
    #[must_use]
    pub const fn feature_revision(self) -> &'static str {
        SUPERGROK_FEATURE_REVISION
    }

    /// Source-review baseline attached to the subscription wire facts.
    #[must_use]
    pub const fn research_baseline(self) -> &'static str {
        XAI_SUBSCRIPTION_RESEARCH_BASELINE
    }

    /// Shared request policy plus the fail-closed SuperGrok deltas it can express.
    ///
    /// The returned closed policy omits `parallel_tool_calls`, emits
    /// `store:false`, requests encrypted reasoning, and uses concise summary.
    #[must_use]
    pub const fn request_policy(self) -> ResponsesRequestPolicy {
        ResponsesRequestPolicy::supergrok_stateless()
    }

    /// SuperGrok never emits the Responses `parallel_tool_calls` field.
    #[must_use]
    pub const fn emits_parallel_tool_calls(self) -> bool {
        false
    }

    /// Whether a response must return encrypted reasoning for the turn to remain usable.
    ///
    /// The official client requests encrypted reasoning and strict local replay.
    #[must_use]
    pub const fn requires_encrypted_reasoning_replay(self) -> bool {
        true
    }

    /// Whether a canonical reasoning effort has an admitted SuperGrok label.
    ///
    /// An omitted effort is supported. The Responses dialect can encode the
    /// Grok 4.5/4.6 union; exact per-model admission remains upstream.
    #[must_use]
    pub fn supports_reasoning_effort(self, effort: Option<&ReasoningEffort>) -> bool {
        matches!(
            effort,
            None | Some(
                ReasoningEffort::Low
                    | ReasoningEffort::Medium
                    | ReasoningEffort::High
                    | ReasoningEffort::XHigh
            )
        )
    }

    /// Typed-only terminal, optional-monotonic sequence, no-DONE stream profile.
    #[must_use]
    pub const fn stream_profile(self) -> ResponsesDialectProfile {
        ResponsesDialectProfile::xai()
    }

    /// Native Grok lifecycle deltas and exact length-limited text snapshots.
    #[must_use]
    pub const fn lifecycle_policy(self) -> LifecycleDialectPolicy {
        LifecycleDialectPolicy::grok()
    }

    /// SuperGrok/xAI response usage requires an exact `total_tokens` field.
    #[must_use]
    pub const fn usage_policy(self) -> ResponsesUsagePolicy {
        ResponsesUsagePolicy::RequireTotal
    }

    /// Complete shared stream construction data for one admitted Grok model.
    #[must_use]
    pub fn stream_data(self, expected_model: &ModelId) -> ResponsesStreamDialectData {
        ResponsesStreamDialectData::grok(expected_model.as_str())
    }

    /// Complete shared replay projection with distinct known/opaque kinds.
    pub fn replay_projection(
        self,
        protocol: ProtocolId,
        model_family: Option<ModelFamilyId>,
        model: ModelId,
    ) -> Result<ResponsesReplayProjection, plakous_responses_dialect::ResponsesProjectError> {
        ResponsesReplayProjection::new(
            protocol,
            model_family,
            model,
            ReplayKindId::new(SUPERGROK_OUTPUT_ITEM_REPLAY_KIND).map_err(|_| {
                plakous_responses_dialect::ResponsesProjectError::ProjectionIdentity
            })?,
            SUPERGROK_OUTPUT_ITEM_REPLAY_VERSION,
        )?
        .requiring_encrypted_reasoning()
        .with_opaque_output_kind(
            ReplayKindId::new(SUPERGROK_OPAQUE_OUTPUT_REPLAY_KIND).map_err(|_| {
                plakous_responses_dialect::ResponsesProjectError::ProjectionIdentity
            })?,
            SUPERGROK_OPAQUE_OUTPUT_REPLAY_VERSION,
        )
    }

    /// Classifies the event-name delta without promoting future events.
    #[must_use]
    pub fn event_delta(self, kind: &str) -> SuperGrokEventDelta {
        match kind {
            "response.created"
            | "response.output_item.added"
            | "response.output_item.done"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done" => SuperGrokEventDelta::Typed,
            "response.completed" | "response.incomplete" | "response.failed" => {
                SuperGrokEventDelta::Terminal
            }
            "error" => SuperGrokEventDelta::ProviderError,
            _ => SuperGrokEventDelta::PreserveOpaque,
        }
    }

    /// Applies only the provider-error labels admitted by the xAI peer path.
    #[must_use]
    pub fn provider_error_class(
        self,
        kind: Option<&str>,
        code: Option<&str>,
    ) -> ResponsesProviderErrorClass {
        match (kind, code) {
            (_, Some("insufficient_quota" | "quota_exceeded")) => {
                ResponsesProviderErrorClass::Quota
            }
            (Some("authentication_error"), _)
            | (_, Some("invalid_api_key" | "authentication_error")) => {
                ResponsesProviderErrorClass::Authentication
            }
            (_, Some("rate_limit_exceeded")) | (Some("rate_limit_error"), _) => {
                ResponsesProviderErrorClass::RateLimited
            }
            (_, Some("server_error")) | (Some("server_error"), _) => {
                ResponsesProviderErrorClass::Server
            }
            _ => ResponsesProviderErrorClass::Unknown,
        }
    }

    /// Replay kind consumed for completed reasoning/output items.
    #[must_use]
    pub const fn output_item_replay(self) -> ResponsesReplayKind<'static> {
        ResponsesReplayKind::new(
            SUPERGROK_OUTPUT_ITEM_REPLAY_KIND,
            SUPERGROK_OUTPUT_ITEM_REPLAY_VERSION,
        )
    }

    /// Replay kind retained for bounded opaque output.
    #[must_use]
    pub const fn opaque_output_replay(self) -> ResponsesReplayKind<'static> {
        ResponsesReplayKind::new(
            SUPERGROK_OPAQUE_OUTPUT_REPLAY_KIND,
            SUPERGROK_OPAQUE_OUTPUT_REPLAY_VERSION,
        )
    }

    /// Encrypted reasoning is carried inside the version-2 output-item replay.
    #[must_use]
    pub const fn encrypted_reasoning_replay(self) -> ResponsesReplayKind<'static> {
        self.output_item_replay()
    }

    /// Exact shared-auth OAuth table, available only with native auth compiled.
    #[cfg(feature = "native-auth")]
    #[must_use]
    pub const fn oauth_pins(self) -> plakous_auth::oauth_pins::SubscriptionOAuthPins {
        crate::oauth_pins::XAI_OAUTH
    }

    /// Exact shared-auth compatibility table, available only with native auth.
    #[cfg(feature = "native-auth")]
    #[must_use]
    pub const fn compatibility_pins(self) -> plakous_auth::oauth_pins::CompatibilityProfilePins {
        crate::oauth_pins::XAI_PROFILE
    }

    /// Exact official Grok Build source revision joined into model admission.
    #[cfg(feature = "native-auth")]
    #[must_use]
    pub const fn official_model_source_revision(self) -> &'static str {
        crate::native_auth::XAI_GROK_BUILD_SOURCE_REVISION
    }
}

/// Built-in closed SuperGrok Responses dialect facts.
pub const SUPERGROK_RESPONSES_DIALECT: SuperGrokResponsesDialect = SuperGrokResponsesDialect::new();

/// Compiles an explicit SuperGrok selection through the exact policy used by
/// the native subscription executor.
///
/// This provider seam validates the pinned route/auth tuple and model-specific
/// effort table before delegating neutral family mechanics to the shared
/// Responses kernel.
#[cfg(feature = "native-auth")]
pub fn compile_supergrok_subscription_configuration(
    selected: &SelectedProviderConfiguration,
    authority: &RuntimeModelAuthority,
    evidence: &crate::XaiModelResolutionEvidence,
) -> Result<CompiledProviderConfiguration, ResponsesProviderConfigurationError> {
    let descriptor = evidence
        .validated_descriptor()
        .map_err(ResponsesProviderConfigurationError::ProviderConfiguration)?
        .clone();
    let route = selected.route();
    if route.protocol().as_str() != SUPERGROK_RESPONSES_DIALECT.protocol_id()
        || route.route().as_str() != SUPERGROK_RESPONSES_DIALECT.route().route_id()
        || route.integration_profile().as_str() != crate::XAI_GROK_COMPATIBILITY_PROFILE
        || route.integration_revision().as_str() != XAI_RESPONSES_ADAPTER_REVISION
        || route.auth_profile().as_str() != crate::XAI_GROK_COMPATIBILITY_PROFILE
        || route.auth_revision().as_str() != crate::XAI_GROK_BUILD_SOURCE_REVISION
        || route.authority_class().as_str() != "oauth-subscription"
        || route.credential_selection().as_str() != "primary"
        || !crate::is_subscription_coding_model_id(route.requested_model().as_str())
        || descriptor.requested_model() != route.requested_model()
    {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "provider_route",
        ));
    }
    let ReasoningEffortDisposition::Enabled(effort) = selected.behavior().reasoning_effort else {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "reasoning_effort",
        ));
    };
    if !crate::subscription_model_reasoning_effort_options(route.requested_model().as_str())
        .contains(&effort)
        || !SUPERGROK_RESPONSES_DIALECT.supports_reasoning_effort(Some(&effort))
    {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "reasoning_effort",
        ));
    }
    require_direct_tool_presentation(selected.behavior().tool_mode)?;
    let semantic_evidence = evidence.semantic_provider_evidence()?;
    let stable_authority =
        ResolvedProviderAuthorityBinding::new(&descriptor, authority, semantic_evidence)?;
    compile_provider_configuration(
        selected,
        descriptor,
        stable_authority,
        SUPERGROK_RESPONSES_DIALECT.request_policy(),
    )
}

#[cfg(feature = "native-auth")]
fn require_direct_tool_presentation(
    tool_mode: ToolPresentationMode,
) -> Result<(), ResponsesProviderConfigurationError> {
    if tool_mode != SUPERGROK_TOOL_PRESENTATION_MODE {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "tool_mode",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use plakous_responses_dialect::{
        ResponsesProviderErrorPolicy, ResponsesResponseLimits, parse_provider_error_with_policy,
    };
    use serde_json::json;

    #[cfg(feature = "native-auth")]
    #[test]
    fn supergrok_rejects_product_composition_tool_presentations() {
        for tool_mode in [
            ToolPresentationMode::LocalComposition,
            ToolPresentationMode::LocalCompositionOnly,
        ] {
            assert!(matches!(
                require_direct_tool_presentation(tool_mode),
                Err(ResponsesProviderConfigurationError::PolicyMismatch(
                    "tool_mode"
                ))
            ));
        }
        assert!(require_direct_tool_presentation(ToolPresentationMode::Direct).is_ok());
    }

    #[test]
    fn request_route_and_revision_facts_match_the_supergrok_golden() {
        let facts = SUPERGROK_RESPONSES_DIALECT;
        assert_eq!(
            json!({
                "route": facts.route().route_id(),
                "protocol": facts.protocol_id(),
                "origin": facts.origin(),
                "path": facts.responses_path(),
                "adapter_revision": facts.adapter_revision(),
                "feature_revision": facts.feature_revision(),
                "research_baseline": facts.research_baseline(),
                "store": false,
                "tool_choice_without_tools": "omit",
                "function_strict": "omit",
                "parallel_tool_calls": "omit",
                "reasoning_include": "reasoning.encrypted_content",
            }),
            json!({
                "route": "xai.grok_subscription",
                "protocol": "xai.responses.grok-subscription.2026-07",
                "origin": "https://cli-chat-proxy.grok.com",
                "path": "/v1/responses",
                "adapter_revision": "plakous-xai-responses-adapter-v7",
                "feature_revision": "plak-xai-subscription-feature-matrix-v8",
                "research_baseline": "2026-08-22",
                "store": false,
                "tool_choice_without_tools": "omit",
                "function_strict": "omit",
                "parallel_tool_calls": "omit",
                "reasoning_include": "reasoning.encrypted_content",
            })
        );
        assert_eq!(
            facts.request_policy(),
            ResponsesRequestPolicy::supergrok_stateless()
        );
        assert!(!facts.emits_parallel_tool_calls());
        assert!(facts.requires_encrypted_reasoning_replay());
    }

    #[test]
    fn only_real_supergrok_reasoning_and_replay_deltas_are_admitted() {
        let facts = SUPERGROK_RESPONSES_DIALECT;
        for effort in [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
        ] {
            assert!(facts.supports_reasoning_effort(Some(&effort)));
        }
        assert!(facts.supports_reasoning_effort(None));
        for effort in [
            ReasoningEffort::Max,
            ReasoningEffort::Ultra,
            ReasoningEffort::TokenBudget(NonZeroU32::MIN),
        ] {
            assert!(!facts.supports_reasoning_effort(Some(&effort)));
        }
        assert_eq!(facts.output_item_replay().name(), "xai.output_item");
        assert_eq!(facts.output_item_replay().version(), 2);
        assert_eq!(
            facts.encrypted_reasoning_replay(),
            facts.output_item_replay()
        );
        assert_eq!(facts.opaque_output_replay().name(), "xai.opaque_output");
        assert_eq!(facts.opaque_output_replay().version(), 1);
    }

    #[test]
    fn event_and_error_deltas_do_not_import_other_responses_surfaces() {
        let facts = SUPERGROK_RESPONSES_DIALECT;
        assert_eq!(
            facts.event_delta("response.reasoning_summary_text.delta"),
            SuperGrokEventDelta::Typed
        );
        assert_eq!(
            facts.event_delta("response.completed"),
            SuperGrokEventDelta::Terminal
        );
        assert_eq!(
            facts.event_delta("error"),
            SuperGrokEventDelta::ProviderError
        );
        for foreign in [
            "response.in_progress",
            "response.output_text.done",
            "response.refusal.delta",
            "response.reasoning_summary_part.added",
            "response.custom_tool_call_input.delta",
            "response.output_text.annotation.added",
        ] {
            assert_eq!(
                facts.event_delta(foreign),
                SuperGrokEventDelta::PreserveOpaque
            );
        }
        assert_eq!(
            facts.provider_error_class(None, Some("invalid_api_key")),
            ResponsesProviderErrorClass::Authentication
        );
        assert_eq!(
            facts.provider_error_class(None, Some("unauthorized")),
            ResponsesProviderErrorClass::Unknown
        );
        assert_eq!(
            facts.provider_error_class(None, Some("internal_error")),
            ResponsesProviderErrorClass::Unknown
        );
        assert_eq!(facts.stream_profile(), ResponsesDialectProfile::xai());
        assert_eq!(facts.lifecycle_policy(), LifecycleDialectPolicy::grok());
        assert_eq!(facts.usage_policy(), ResponsesUsagePolicy::RequireTotal);
    }

    #[test]
    fn provider_error_payloads_use_only_the_exact_grok_allowlist()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = ResponsesResponseLimits::default();
        for (payload, expected) in [
            (
                json!({"code":"invalid_api_key","message":"denied"}),
                ResponsesProviderErrorClass::Authentication,
            ),
            (
                json!({"type":"authentication_error","message":"denied"}),
                ResponsesProviderErrorClass::Authentication,
            ),
            (
                json!({"code":"quota_exceeded","message":"quota"}),
                ResponsesProviderErrorClass::Quota,
            ),
            (
                json!({"code":"rate_limit_exceeded","message":"slow"}),
                ResponsesProviderErrorClass::RateLimited,
            ),
            (
                json!({"type":"rate_limit_error","message":"slow"}),
                ResponsesProviderErrorClass::RateLimited,
            ),
            (
                json!({"type":"server_error","message":"later"}),
                ResponsesProviderErrorClass::Server,
            ),
            (
                json!({"code":"unauthorized","message":"foreign near match"}),
                ResponsesProviderErrorClass::Unknown,
            ),
            (
                json!({"code":"internal_error","message":"foreign near match"}),
                ResponsesProviderErrorClass::Unknown,
            ),
        ] {
            let parsed = parse_provider_error_with_policy(
                &payload,
                limits,
                ResponsesProviderErrorPolicy::Grok,
            )?;
            assert_eq!(parsed.class(), expected);
            assert_eq!(parsed.native(), &payload);
        }
        Ok(())
    }

    #[test]
    fn shared_stream_and_replay_tables_are_complete_and_distinct()
    -> Result<(), Box<dyn std::error::Error>> {
        let facts = SUPERGROK_RESPONSES_DIALECT;
        let model = ModelId::new("grok-test")?;
        let stream = facts.stream_data(&model);
        assert_eq!(stream.usage_policy(), ResponsesUsagePolicy::RequireTotal);

        let protocol = ProtocolId::new(facts.protocol_id())?;
        let projection = facts.replay_projection(protocol, None, model)?;
        assert_eq!(
            projection.output_kind().as_str(),
            SUPERGROK_OUTPUT_ITEM_REPLAY_KIND
        );
        assert_eq!(
            projection.output_kind_version(),
            SUPERGROK_OUTPUT_ITEM_REPLAY_VERSION
        );
        assert_eq!(
            projection.opaque_output_kind().map(ReplayKindId::as_str),
            Some(SUPERGROK_OPAQUE_OUTPUT_REPLAY_KIND)
        );
        assert_eq!(
            projection.opaque_output_kind_version(),
            Some(SUPERGROK_OPAQUE_OUTPUT_REPLAY_VERSION)
        );
        Ok(())
    }

    #[cfg(feature = "native-auth")]
    #[test]
    fn auth_tables_and_source_revisions_are_exact() {
        let facts = SUPERGROK_RESPONSES_DIALECT;
        let oauth = facts.oauth_pins();
        assert_eq!(oauth.client_id, "b1a00492-073a-47ea-816f-4c329264a828");
        assert_eq!(oauth.auth_origin, "https://auth.x.ai");
        assert_eq!(
            oauth.oidc_discovery_url,
            Some("https://auth.x.ai/.well-known/openid-configuration")
        );
        assert_eq!(
            oauth.scopes,
            "openid profile email offline_access grok-cli:access api:access"
        );
        assert_eq!(oauth.authorize_path, "/oauth2/authorize");
        assert_eq!(oauth.token_path, "/oauth2/token");
        assert_eq!(oauth.device_authorization_path, "/oauth2/device/code");
        assert_eq!(
            oauth.loopback_redirect_uri,
            "http://127.0.0.1:56121/callback"
        );

        let profile = facts.compatibility_pins();
        assert_eq!(profile.profile_id, "xai.grok.compat.2026-07-31.r1");
        assert_eq!(
            profile.source_revision,
            "19231fce4b70aa5f7894a0a0eb20ff29bd417db5"
        );
        assert_eq!(
            profile.notice_revision,
            "xai.grok.compat.notice.2026-07-31.r1"
        );
        assert_eq!(
            profile.kill_switch_revision,
            "xai.grok.compat.kill.2026-07-31.r1"
        );
        assert_eq!(profile.reviewed_at_unix, 1_785_456_000);
        assert_eq!(profile.expires_at_unix, 1_801_353_600);
        assert!(!profile.killed);
        assert_eq!(
            facts.official_model_source_revision(),
            "afbc0fb710320c7add294c2106d447ecc3e3af2e"
        );
    }

    #[test]
    fn subscription_authority_is_exact() {
        assert_eq!(
            SUPERGROK_SUBSCRIPTION_BASE_URL,
            format!("{SUPERGROK_RESPONSES_ORIGIN}/v1")
        );
        assert_eq!(SUPERGROK_MODELS_PATH, "/v1/models");
        assert_eq!(SUPERGROK_RESPONSES_PATH, "/v1/responses");
    }
}
