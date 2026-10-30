use std::time::Duration;

use plakous_attempt::{FamilyCoreError, RetryClass, TerminalFacts};
use plakous_http::{ErrorKind as HttpErrorKind, Failure as HttpFailure, StatusCode};

use crate::project_host::ResponsesProjectError;
use crate::request_host::ResponsesRequestError;
use crate::response_host::ResponsesProviderError;
use crate::stream_host::ResponsesFamilyStreamError;

use super::{ResponsesDispatchRejection, ResponsesPhysicalDispatchBinding};

/// Failure to build an exact attempt or service configuration.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResponsesAttemptBuildError {
    /// Shared request serialization failed.
    #[error("Responses request serialization failed: {0}")]
    Request(#[from] ResponsesRequestError),
    /// An identity used the wrong digest domain or could not be constructed.
    #[error("Responses attempt binding is invalid")]
    Binding,
    /// The selected ledger cannot reserve every required authorization slot.
    #[error("Responses authorization slots are incompatible with the attempt ledger")]
    AuthSlots,
    /// The dialect behavior revision was zero or a closed datum was unsupported.
    #[error("Responses attempt dialect behavior identity is invalid")]
    DialectIdentity,
    /// Terminal replay projection identity construction failed.
    #[error("Responses replay projection identity is invalid")]
    Projection(#[from] ResponsesProjectError),
}

impl ResponsesAttemptBuildError {
    /// Classifies protected native history separately from ordinary projection failures.
    #[must_use]
    pub const fn preparation_error_kind(&self) -> plakous_model::ModelPreparationErrorKind {
        match self {
            Self::Request(ResponsesRequestError::Content(
                crate::ResponsesRequestContentError::CompactionWithoutReplay
                | crate::ResponsesRequestContentError::AssistantRefusalWithoutReplay
                | crate::ResponsesRequestContentError::OpaqueOnlyWithoutCompatibleReplay
                | crate::ResponsesRequestContentError::ProviderHostedCallWithoutReplay,
            )) => plakous_model::ModelPreparationErrorKind::ProtectedHistory,
            _ => plakous_model::ModelPreparationErrorKind::Projection,
        }
    }
}

/// Stable classification for a non-successful HTTP response head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponsesHttpStatusClass {
    /// HTTP 401.
    Authentication,
    /// HTTP 403.
    Authorization,
    /// HTTP 429.
    RateLimited,
    /// HTTP 402.
    Quota,
    /// HTTP 413.
    ContextLimit,
    /// Other 4xx response (acceptance remains ambiguous without dialect proof).
    Rejected,
    /// 5xx ambiguous server outcome.
    Server,
    /// HTTP 401 whose body is a route's proven wrap of the provider's upstream
    /// HTTP 503 overload refusal; it is refused before acceptance and retried
    /// as that 503, never as a credential rejection.
    WrappedOverload,
    /// Other unsupported/ambiguous status.
    Unknown,
}

/// Terminal host failure with conservative retry/effect facts.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResponsesAttemptError {
    /// Service and attempt execution data differed.
    #[error("Responses attempt does not match the admitted service")]
    Binding {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Conservative facts at rejection.
        facts: TerminalFacts,
    },
    /// Attempt/physical observation ordering failed closed.
    #[error("Responses attempt observation failed")]
    Observation {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Shared lifecycle/observation failure.
        source: FamilyCoreError,
        /// Conservative facts at rejection.
        facts: TerminalFacts,
    },
    /// Dialect preflight rejected before a physical request.
    #[error("Responses preflight rejected dispatch")]
    Preflight {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Closed dialect-owned denial.
        rejection: ResponsesDispatchRejection,
        /// Conservative facts at rejection.
        facts: TerminalFacts,
    },
    /// Dialect revalidation rejected the exact physical request.
    #[error("Responses final revalidation rejected dispatch")]
    Revalidation {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Closed dialect-owned denial.
        rejection: ResponsesDispatchRejection,
        /// Conservative facts at rejection.
        facts: TerminalFacts,
    },
    /// Cooperative cancellation was acknowledged.
    #[error("Responses attempt was cancelled")]
    Cancelled {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Admission-sensitive terminal facts.
        facts: TerminalFacts,
    },
    /// The dispatch deadline or the stream idle bound elapsed.
    #[error("Responses attempt deadline elapsed")]
    Deadline {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Admission-sensitive terminal facts.
        facts: TerminalFacts,
    },
    /// The preflight request body or physical generation was substituted.
    #[error("Responses preflight substituted the bound physical request")]
    RequestBinding {
        /// Exact physical binding when a generation had started.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Conservative facts at rejection.
        facts: TerminalFacts,
    },
    /// Provider-blind transport failed.
    #[error("Responses HTTP transport failed")]
    Transport {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Redacted phase/transmission failure.
        source: HttpFailure,
        /// Conservative physical-attempt facts.
        facts: TerminalFacts,
    },
    /// A non-success HTTP status was classified without fallback.
    #[error("Responses provider rejected or ambiguously failed the HTTP request")]
    HttpStatus {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Exact non-success HTTP status.
        status: StatusCode,
        /// Stable family classification.
        class: ResponsesHttpStatusClass,
        /// Optional bounded typed provider error.
        provider: Option<ResponsesProviderError>,
        /// Provider `Retry-After` advice in whole seconds, when present.
        retry_after: Option<Duration>,
        /// Provider advice or a body-classified entitlement failure forbids replay.
        retry_disabled: bool,
        /// Conservative physical-attempt facts.
        facts: TerminalFacts,
    },
    /// Successful HTTP status did not carry the admitted SSE media type.
    #[error("Responses success did not use the SSE media type")]
    ContentType {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Conservative accepted-request facts.
        facts: TerminalFacts,
    },
    /// Successful response head carried invalid request-attribution metadata.
    #[error("Responses response request attribution is invalid")]
    ResponseMetadata {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Conservative accepted-request facts.
        facts: TerminalFacts,
    },
    /// Shared family SSE/lifecycle decoding failed.
    #[error("Responses stream decoding failed")]
    Stream {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Shared family decoder failure.
        source: ResponsesFamilyStreamError,
        /// Conservative accepted-request facts.
        facts: TerminalFacts,
    },
    /// More than one typed terminal escaped the shared stream kernel.
    #[error("Responses stream produced more than one typed terminal")]
    MultipleTerminals {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Conservative accepted-request facts.
        facts: TerminalFacts,
    },
    /// Terminal projection/artifact preparation failed.
    #[error("Responses terminal projection failed")]
    Projection {
        /// Exact host-verified physical binding.
        physical: Option<Box<ResponsesPhysicalDispatchBinding>>,
        /// Terminal projection/artifact failure.
        source: ResponsesProjectError,
        /// Conservative accepted-request facts.
        facts: TerminalFacts,
    },
}

impl ResponsesAttemptError {
    pub(super) fn with_summary(mut self, summary: plakous_attempt::AttemptSummary) -> Self {
        match &mut self {
            Self::Binding { facts, .. }
            | Self::Observation { facts, .. }
            | Self::Preflight { facts, .. }
            | Self::Revalidation { facts, .. }
            | Self::Cancelled { facts, .. }
            | Self::Deadline { facts, .. }
            | Self::RequestBinding { facts, .. }
            | Self::Transport { facts, .. }
            | Self::HttpStatus { facts, .. }
            | Self::ContentType { facts, .. }
            | Self::ResponseMetadata { facts, .. }
            | Self::Stream { facts, .. }
            | Self::MultipleTerminals { facts, .. }
            | Self::Projection { facts, .. } => *facts = facts.with_summary(summary),
        }
        self
    }

    /// Returns conservative physical-attempt facts.
    #[must_use]
    pub const fn facts(&self) -> TerminalFacts {
        match self {
            Self::Binding { facts, .. }
            | Self::Observation { facts, .. }
            | Self::Preflight { facts, .. }
            | Self::Revalidation { facts, .. }
            | Self::Cancelled { facts, .. }
            | Self::Deadline { facts, .. }
            | Self::RequestBinding { facts, .. }
            | Self::Transport { facts, .. }
            | Self::HttpStatus { facts, .. }
            | Self::ContentType { facts, .. }
            | Self::ResponseMetadata { facts, .. }
            | Self::Stream { facts, .. }
            | Self::MultipleTerminals { facts, .. }
            | Self::Projection { facts, .. } => *facts,
        }
    }

    /// Returns the exact physical dispatch binding for every post-start result.
    #[must_use]
    pub fn physical(&self) -> Option<&ResponsesPhysicalDispatchBinding> {
        match self {
            Self::Binding { physical, .. }
            | Self::Observation { physical, .. }
            | Self::Preflight { physical, .. }
            | Self::Revalidation { physical, .. }
            | Self::Cancelled { physical, .. }
            | Self::Deadline { physical, .. }
            | Self::RequestBinding { physical, .. }
            | Self::Transport { physical, .. }
            | Self::HttpStatus { physical, .. }
            | Self::ContentType { physical, .. }
            | Self::ResponseMetadata { physical, .. }
            | Self::Stream { physical, .. }
            | Self::MultipleTerminals { physical, .. }
            | Self::Projection { physical, .. } => physical.as_deref(),
        }
    }

    /// Returns the strongest safe retry classification. The host never retries
    /// itself. Deterministic local cancellation/limit failures remain `Never`
    /// even when the separately exposed acceptance facts retain duplicate-risk
    /// uncertainty after final authorization.
    #[must_use]
    pub const fn retry_class(&self) -> RetryClass {
        match self {
            Self::Cancelled { .. }
            | Self::Deadline { .. }
            | Self::Binding { .. }
            | Self::RequestBinding { .. }
            | Self::ContentType { .. }
            | Self::ResponseMetadata { .. }
            | Self::Stream { .. }
            | Self::MultipleTerminals { .. }
            | Self::Projection { .. }
            | Self::HttpStatus {
                class: ResponsesHttpStatusClass::Quota,
                ..
            } => RetryClass::Never,
            Self::Transport { source, facts, .. }
                if matches!(
                    source.kind(),
                    HttpErrorKind::Cancelled | HttpErrorKind::LimitExceeded(_)
                ) =>
            {
                let _ = facts;
                RetryClass::Never
            }
            Self::Observation { facts, .. }
            | Self::Preflight { facts, .. }
            | Self::Revalidation { facts, .. }
            | Self::Transport { facts, .. }
            | Self::HttpStatus { facts, .. } => facts.retry_class(),
        }
    }
}
