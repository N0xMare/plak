//! The neutral port a hosted-retrieval wrapper tool calls.
//!
//! A wrapper tool is a local read-only tool. Each call issues one isolated
//! side request on the session's own route, account, credential and model;
//! the route's provider runs the search or fetch inside that request. The
//! side request never streams into the transcript and never enters canonical
//! history: only the wrapper's tool result does. A failure after the
//! kernel's bounded retries is the tool's error result.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::ExecutionOperationControl;

/// The retrieval a port can perform.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RetrievalKind {
    /// Web search.
    WebSearch,
    /// Fetch of one page the request names.
    PageFetch,
    /// Search of posts on a social platform the route's provider indexes.
    PostSearch,
}

/// Searches one web search call may run unless an operator sets fewer or more.
pub const DEFAULT_WEB_SEARCH_MAX_USES: u8 = 5;

/// One retrieval, as the wrapper's validated arguments state it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetrievalRequest {
    /// Search the web.
    WebSearch {
        /// The query.
        query: String,
        /// Only results from these domains.
        allowed_domains: Vec<String>,
        /// No results from these domains.
        blocked_domains: Vec<String>,
        /// Searches the provider may run for this one call.
        max_uses: u8,
    },
    /// Fetch one page and answer about it.
    PageFetch {
        /// The absolute URL.
        url: String,
        /// What to extract from the page.
        prompt: Option<String>,
    },
    /// Search posts.
    PostSearch {
        /// The query.
        query: String,
        /// Only posts by these handles.
        handles: Vec<String>,
        /// Earliest post date, `YYYY-MM-DD`.
        from_date: Option<String>,
        /// Latest post date, `YYYY-MM-DD`.
        to_date: Option<String>,
    },
}

impl RetrievalRequest {
    /// The kind of retrieval this request asks for.
    #[must_use]
    pub const fn kind(&self) -> RetrievalKind {
        match self {
            Self::WebSearch { .. } => RetrievalKind::WebSearch,
            Self::PageFetch { .. } => RetrievalKind::PageFetch,
            Self::PostSearch { .. } => RetrievalKind::PostSearch,
        }
    }
}

/// One source the provider reported for the answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetrievalSource {
    /// The source URL.
    pub url: String,
    /// The page title, when the provider reported one.
    pub title: Option<String>,
}

/// What one side request consumed, attributed to the tool call.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetrievalUsage {
    /// Input tokens over every physical exchange of the side request.
    pub input_tokens: u64,
    /// Output tokens over every physical exchange of the side request.
    pub output_tokens: u64,
    /// Hosted retrievals the provider counted (searches or fetches).
    pub hosted_requests: u64,
}

/// The completed retrieval.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RetrievalOutcome {
    /// The side model's answer.
    pub text: String,
    /// Sources in first-reported order, without duplicate URLs.
    pub sources: Vec<RetrievalSource>,
    /// What the side request consumed.
    pub usage: RetrievalUsage,
    /// Physical requests the side request sent: every exchange with the
    /// provider, retries and `pause_turn` continuations included. `None`
    /// when the port cannot say, which a helper budget charges as the
    /// port's whole declared maximum.
    pub physical_requests: Option<u32>,
}

/// Why a retrieval failed; the wrapper returns it as its tool error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RetrievalErrorKind {
    /// The port does not serve this kind of retrieval.
    Unsupported,
    /// The request is malformed or exceeds a bound.
    InvalidRequest,
    /// Admission or the credential is no longer current.
    Unauthorized,
    /// The provider refused the request.
    Rejected,
    /// The provider could not be reached or kept failing after retries.
    Unavailable,
    /// The provider's response broke the expected grammar.
    InvalidResponse,
    /// The call was cancelled.
    Cancelled,
}

/// A failed retrieval with a redaction-safe one-line message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetrievalError {
    kind: RetrievalErrorKind,
    message: String,
}

impl RetrievalError {
    /// Creates a failure; the message must not carry credentials.
    #[must_use]
    pub fn new(kind: RetrievalErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The failure class.
    #[must_use]
    pub const fn kind(&self) -> RetrievalErrorKind {
        self.kind
    }

    /// The one-line message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RetrievalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RetrievalError {}

/// The future one retrieval returns.
pub type RetrievalFuture =
    Pin<Box<dyn Future<Output = Result<RetrievalOutcome, RetrievalError>> + Send + 'static>>;

/// One route's hosted retrieval, bound to the session's exact route, account,
/// credential and model.
///
/// An implementation revalidates admission immediately before each side
/// request, as before a model round, and never falls back to another route
/// or account.
pub trait RetrievalPort: Send + Sync {
    /// The retrievals this port serves.
    fn kinds(&self) -> &[RetrievalKind];

    /// The most physical requests one retrieval of `kind` can send,
    /// retries and continuations included; `None` when the port declares
    /// no bound. A helper budget (spec 01 §9.8.7) reserves this before the
    /// call, so a wrapper over a port without a declaration cannot be
    /// registered for a composition that spends under one.
    fn max_requests(&self, kind: RetrievalKind) -> Option<u32> {
        let _ = kind;
        None
    }

    /// Runs one retrieval as one isolated side request.
    fn retrieve(
        &self,
        request: RetrievalRequest,
        control: ExecutionOperationControl,
    ) -> RetrievalFuture;
}
