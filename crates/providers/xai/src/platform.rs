//! xAI Platform authority and documentation facts.
//!
//! Executable Grok 4.6 binding lives in the Platform model; the separate hosted
//! code witness remains request-only evidence and grants no attachment.

use std::fmt;

use plakous_model::{
    CompiledProviderConfiguration, Digest, DigestDomain, HostedTool, ModelId,
    SelectedProviderConfiguration,
};
use serde::Serialize;
use thiserror::Error;

use crate::{XaiDialect, XaiRouteKind};

/// Exact first-party xAI Platform inference origin.
pub(crate) const XAI_PLATFORM_API_KEY_ORIGIN: &str = "https://api.x.ai";
/// Exact create-response resource path.
pub(crate) const XAI_PLATFORM_RESPONSES_PATH: &str = "/v1/responses";
/// Closed integration policy revision.
pub(crate) const XAI_PLATFORM_POLICY_REVISION: &str = "plakous-xai-platform-2026-08-12-r1";
/// Earlier model retained only as documentation reference.
pub(crate) const XAI_PLATFORM_GROK_45_MODEL: &str = "grok-4.5";
/// Documentation-reported context for the exact model, retained as reference evidence only.
pub(crate) const XAI_PLATFORM_GROK_45_REFERENCE_CONTEXT_TOKENS: u32 = 500_000;
/// Exact model admitted for Platform execution and the separate hosted-code witness.
pub const XAI_PLATFORM_GROK_46_MODEL: &str = "grok-4.6";
/// Input window of the admitted model: `context_length` in the authenticated
/// `GET /v1/models` row for `grok-4.6` (observed 2026-09-26).
pub const XAI_PLATFORM_GROK_46_CONTEXT_TOKENS: u64 = 500_000;
/// Exact offline-only hosted-code witness profile; it grants no product attachment.
pub const XAI_PLATFORM_GROK_46_HOSTED_CODE_PROFILE: &str =
    "xai.platform.grok-4.6.code-interpreter.2026-08-23-r1";
/// Review identity for the bounded request-capability hosted-code evidence.
pub const XAI_PLATFORM_GROK_46_HOSTED_CODE_REVISION: &str =
    "plakous-xai-platform-grok-4.6-code-interpreter-2026-08-23-r1";
/// Exact request tool discriminator supported by the reviewed hosted-code witness.
pub(crate) const XAI_PLATFORM_CODE_INTERPRETER_TOOL_TYPE: &str = "code_interpreter";
/// Documented create-response default/output ceiling used as an explicit local cap.
pub const XAI_PLATFORM_MAX_OUTPUT_TOKENS: u32 = 128_000;
/// Documented selector required for replayable local reasoning history.
pub(crate) const XAI_PLATFORM_ENCRYPTED_REASONING_INCLUDE: &str = "reasoning.encrypted_content";
/// API-key auth profile identity retained by shared configuration authority.
pub const XAI_PLATFORM_API_KEY_AUTH_PROFILE: &str = "xai.platform.api-key";
/// API-key auth semantics revision retained by shared configuration authority.
pub const XAI_PLATFORM_API_KEY_AUTH_REVISION: &str = "xai.platform.api-key.2026-08-12-r1";
/// Exact paid authority class for the separately billed route.
pub const XAI_PLATFORM_API_KEY_AUTHORITY_CLASS: &str = "api-key-platform";
/// Explicit non-secret selector for Platform credentials.
pub const XAI_PLATFORM_API_KEY_CREDENTIAL_SELECTION: &str = "primary";

/// Exact API-key authority identity. No secret is stored or rendered.
#[derive(Clone, Eq, PartialEq)]
pub struct XaiPlatformApiKeyAuthority {
    account_binding: Digest,
    credential_authority_binding: Digest,
    generation: u64,
}

impl XaiPlatformApiKeyAuthority {
    /// Binds an embedder-owned account/credential selector without accepting a key.
    pub fn new(
        account: &str,
        credential_authority: &str,
        generation: u64,
    ) -> Result<Self, XaiPlatformConfigurationError> {
        if !valid_authority_label(account)
            || !valid_authority_label(credential_authority)
            || generation == 0
        {
            return Err(XaiPlatformConfigurationError::InvalidApiKeyAuthority);
        }
        Ok(Self {
            account_binding: Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"plakous-xai-platform-account-v1", account.as_bytes()],
            )?,
            credential_authority_binding: Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[
                    b"plakous-xai-platform-api-key-authority-v1",
                    credential_authority.as_bytes(),
                ],
            )?,
            generation,
        })
    }

    pub(crate) fn for_key_generation(
        &self,
        fingerprint: &[u8],
    ) -> Result<Self, XaiPlatformConfigurationError> {
        Ok(Self {
            account_binding: Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[
                    b"xai-platform-key-account-v1",
                    self.account_binding.bytes(),
                    fingerprint,
                ],
            )?,
            credential_authority_binding: Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[
                    b"xai-platform-key-authority-v1",
                    self.credential_authority_binding.bytes(),
                    fingerprint,
                ],
            )?,
            generation: self.generation,
        })
    }

    /// Non-secret stable account binding.
    #[must_use]
    pub const fn account_binding(&self) -> Digest {
        self.account_binding
    }
    /// Non-secret stable credential-authority binding.
    #[must_use]
    pub const fn credential_authority_binding(&self) -> Digest {
        self.credential_authority_binding
    }
    /// Current embedder-owned credential generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

impl fmt::Debug for XaiPlatformApiKeyAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XaiPlatformApiKeyAuthority")
            .field("account_binding", &"<redacted digest>")
            .field("credential_authority_binding", &"<redacted digest>")
            .field("generation", &self.generation)
            .finish()
    }
}

fn valid_authority_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/".contains(&byte))
}

/// Closed documentation evidence for the exact public Platform model page.
///
/// This is neither authenticated discovery nor a provider capacity guarantee.
/// It is retained only as request-lowering research and never creates a context
/// contract or executable model descriptor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct XaiPlatformDocumentationWitness {
    model: ModelId,
    context_tokens: u32,
    source_revision: &'static str,
    witness_identity: Digest,
}

impl XaiPlatformDocumentationWitness {
    /// Returns the reviewed, credential-free Grok 4.5 documentation witness.
    pub fn reviewed_grok_45() -> Result<Self, XaiPlatformConfigurationError> {
        let model = ModelId::new(XAI_PLATFORM_GROK_45_MODEL)?;
        let witness_identity = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-xai-platform-catalog-witness-v1",
                XAI_PLATFORM_GROK_45_MODEL.as_bytes(),
                &XAI_PLATFORM_GROK_45_REFERENCE_CONTEXT_TOKENS.to_be_bytes(),
                XAI_PLATFORM_POLICY_REVISION.as_bytes(),
            ],
        )?;
        Ok(Self {
            model,
            context_tokens: XAI_PLATFORM_GROK_45_REFERENCE_CONTEXT_TOKENS,
            source_revision: XAI_PLATFORM_POLICY_REVISION,
            witness_identity,
        })
    }

    /// Exact requested/effective model attested by this closed row.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }
    /// Documentation-reported context, not a provider guarantee.
    #[must_use]
    pub const fn reference_context_tokens(&self) -> u32 {
        self.context_tokens
    }
    /// Reviewed source/policy revision.
    #[must_use]
    pub const fn source_revision(&self) -> &'static str {
        self.source_revision
    }
    /// Credential-free witness identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.witness_identity
    }
}

/// Credential-free Grok 4.6 code-interpreter request-capability witness.
///
/// xAI exposes no native PTC caller edge. This provider-hosted facility is only
/// evidence for a future optional Plak-owned extension, not a Plakous default.
/// No response discriminator, member schema, stdout, container, or stream-event
/// contract is claimed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct XaiPlatformHostedCodeWitness {
    model: ModelId,
    source_revision: &'static str,
    witness_identity: Digest,
}

impl XaiPlatformHostedCodeWitness {
    /// Returns the reviewed request-capability Grok 4.6 witness.
    pub fn reviewed_grok_46() -> Result<Self, XaiPlatformConfigurationError> {
        let model = ModelId::new(XAI_PLATFORM_GROK_46_MODEL)?;
        let witness_identity = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-xai-platform-hosted-code-witness-v1",
                XAI_PLATFORM_GROK_46_MODEL.as_bytes(),
                XAI_PLATFORM_CODE_INTERPRETER_TOOL_TYPE.as_bytes(),
                XAI_PLATFORM_GROK_46_HOSTED_CODE_REVISION.as_bytes(),
            ],
        )?;
        Ok(Self {
            model,
            source_revision: XAI_PLATFORM_GROK_46_HOSTED_CODE_REVISION,
            witness_identity,
        })
    }

    /// Exact documented model.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }
    /// Exact reviewed evidence revision.
    #[must_use]
    pub const fn source_revision(&self) -> &'static str {
        self.source_revision
    }
    /// Credential-free evidence identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.witness_identity
    }
}

/// Validates the exact Grok 4.6 witness selection and refuses product attachment.
///
/// Direct remains the xAI default. This witness does not implement the distinct
/// optional Plak-owned hosted-code extension and never returns a configuration.
pub fn compile_xai_platform_grok_46_hosted_code_configuration(
    selected: &SelectedProviderConfiguration,
    witness: XaiPlatformHostedCodeWitness,
    authority: &XaiPlatformApiKeyAuthority,
) -> Result<CompiledProviderConfiguration, XaiPlatformConfigurationError> {
    if witness != XaiPlatformHostedCodeWitness::reviewed_grok_46()? {
        return Err(XaiPlatformConfigurationError::PolicyMismatch(
            "hosted_code_witness",
        ));
    }
    let route = selected.route();
    let behavior = selected.behavior();
    if route.protocol().as_str() != XaiDialect::PlatformResponses2026_08.protocol_id()
        || route.route().as_str() != XaiRouteKind::PlatformApiKey.route_id()
        || route.integration_profile().as_str() != XAI_PLATFORM_GROK_46_HOSTED_CODE_PROFILE
        || route.integration_revision().as_str() != XAI_PLATFORM_GROK_46_HOSTED_CODE_REVISION
        || route.auth_profile().as_str() != XAI_PLATFORM_API_KEY_AUTH_PROFILE
        || route.auth_revision().as_str() != XAI_PLATFORM_API_KEY_AUTH_REVISION
        || route.authority_class().as_str() != XAI_PLATFORM_API_KEY_AUTHORITY_CLASS
        || route.credential_selection().as_str() != XAI_PLATFORM_API_KEY_CREDENTIAL_SELECTION
        || route.requested_model().as_str() != XAI_PLATFORM_GROK_46_MODEL
        || behavior.hosted_tools != [HostedTool::CodeInterpreter]
        || !behavior.tools.is_empty()
    {
        return Err(XaiPlatformConfigurationError::PolicyMismatch(
            "hosted_code_profile",
        ));
    }
    let _ = (witness, authority);
    Err(XaiPlatformConfigurationError::HostedCodeProductExtensionDisabled)
}

/// Documentation-backed facts used only to check deterministic request lowering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XaiPlatformRequestLoweringFacts;

impl XaiPlatformRequestLoweringFacts {
    /// Exact documented model slug.
    #[must_use]
    pub const fn model(self) -> &'static str {
        XAI_PLATFORM_GROK_45_MODEL
    }

    /// Exact documented encrypted-reasoning include selector.
    #[must_use]
    pub const fn reasoning_include(self) -> &'static str {
        XAI_PLATFORM_ENCRYPTED_REASONING_INCLUDE
    }

    /// Whether the documentation supports emitting a streaming request field.
    #[must_use]
    pub const fn stream_request(self) -> bool {
        true
    }

    /// Documentation-reported context value, explicitly not admission authority.
    #[must_use]
    pub const fn reference_context_tokens(self) -> u32 {
        XAI_PLATFORM_GROK_45_REFERENCE_CONTEXT_TOKENS
    }
}

/// Returns request-lowering research without creating an executable descriptor.
#[must_use]
pub const fn xai_platform_request_lowering_facts() -> XaiPlatformRequestLoweringFacts {
    XaiPlatformRequestLoweringFacts
}

/// Documented public SSE event disposition; no undocumented lifecycle is promoted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XaiPlatformStreamEventPolicy {
    /// Documented visible text delta.
    TextDelta,
    /// Documented raw reasoning text delta.
    ReasoningTextDelta,
    /// Documented reasoning summary text delta.
    ReasoningSummaryDelta,
    /// Bounded observational retention only; no semantic claim.
    PreserveUnknown,
}

/// Returns the exact event policy proven by current public documentation.
#[must_use]
pub fn xai_platform_stream_event_policy(event: &str) -> XaiPlatformStreamEventPolicy {
    match event {
        "response.output_text.delta" => XaiPlatformStreamEventPolicy::TextDelta,
        "response.reasoning_text.delta" => XaiPlatformStreamEventPolicy::ReasoningTextDelta,
        "response.reasoning_summary_text.delta" => {
            XaiPlatformStreamEventPolicy::ReasoningSummaryDelta
        }
        _ => XaiPlatformStreamEventPolicy::PreserveUnknown,
    }
}

/// Conservative documented HTTP status classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XaiPlatformHttpErrorClass {
    /// HTTP 400 invalid request.
    InvalidRequest,
    /// HTTP 401 authentication failure.
    Authentication,
    /// HTTP 403 permission failure.
    Permission,
    /// HTTP 404 model or endpoint not found.
    NotFound,
    /// HTTP 405 wrong method.
    Method,
    /// HTTP 415 content-type/body failure.
    ContentType,
    /// HTTP 422 invalid field format.
    InvalidField,
    /// HTTP 429 inference rate limit.
    RateLimited,
    /// Status without a documented exact classification.
    Unknown,
}

/// Classifies only documented xAI public HTTP status values.
#[must_use]
pub const fn xai_platform_http_error_policy(status: u16) -> XaiPlatformHttpErrorClass {
    match status {
        400 => XaiPlatformHttpErrorClass::InvalidRequest,
        401 => XaiPlatformHttpErrorClass::Authentication,
        403 => XaiPlatformHttpErrorClass::Permission,
        404 => XaiPlatformHttpErrorClass::NotFound,
        405 => XaiPlatformHttpErrorClass::Method,
        415 => XaiPlatformHttpErrorClass::ContentType,
        422 => XaiPlatformHttpErrorClass::InvalidField,
        429 => XaiPlatformHttpErrorClass::RateLimited,
        _ => XaiPlatformHttpErrorClass::Unknown,
    }
}

/// Closed configuration failure.
#[derive(Debug, Error)]
pub enum XaiPlatformConfigurationError {
    /// Account, credential selector, or generation was invalid.
    #[error("invalid xAI Platform API-key authority identity")]
    InvalidApiKeyAuthority,
    /// One exact selected/catalog/policy field did not match the closed profile.
    #[error("xAI Platform policy mismatch: {0}")]
    PolicyMismatch(&'static str),
    /// The hosted-code witness is not an implemented or default product attachment.
    #[error("xAI hosted code is an optional Plak-owned product extension that is not implemented")]
    HostedCodeProductExtensionDisabled,
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] plakous_model::DigestError),
    /// Canonical identity construction failed.
    #[error(transparent)]
    Identity(#[from] plakous_model::IdentityError),
}
