//! Release-owned facts about which provider-hosted retrieval tools a route
//! serves.
//!
//! Each provider crate states one [`HostedRetrievalTable`] per route it owns.
//! A row moves to [`HostedRetrievalSupport::Supported`] only with a reviewed
//! live qualification; binding never probes a route to find out.

/// A retrieval tool the route's provider executes inside a side request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HostedRetrievalTool {
    /// Web search.
    WebSearch,
    /// Fetch of one web page.
    WebFetch,
    /// Search of X posts.
    XSearch,
}

/// Why a route does not serve a hosted retrieval tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostedRetrievalUnsupported {
    /// The route's provider offers no such tool.
    NotOffered,
    /// The provider offers the tool, but no live qualification of it on this
    /// route has been recorded.
    NotQualified,
}

/// Whether one route serves one hosted retrieval tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostedRetrievalSupport {
    /// Qualified on this route.
    Supported,
    /// Refused at bind.
    Unsupported {
        /// Why the route refuses it.
        reason: HostedRetrievalUnsupported,
    },
}

impl HostedRetrievalSupport {
    /// The provider offers no such tool on this route.
    pub const NOT_OFFERED: Self = Self::Unsupported {
        reason: HostedRetrievalUnsupported::NotOffered,
    };
    /// The provider offers the tool; it is not yet qualified on this route.
    pub const NOT_QUALIFIED: Self = Self::Unsupported {
        reason: HostedRetrievalUnsupported::NotQualified,
    };
}

/// One route's hosted retrieval support, one row per tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostedRetrievalTable {
    web_search: HostedRetrievalSupport,
    web_fetch: HostedRetrievalSupport,
    x_search: HostedRetrievalSupport,
}

impl HostedRetrievalTable {
    /// States every row of one route.
    #[must_use]
    pub const fn new(
        web_search: HostedRetrievalSupport,
        web_fetch: HostedRetrievalSupport,
        x_search: HostedRetrievalSupport,
    ) -> Self {
        Self {
            web_search,
            web_fetch,
            x_search,
        }
    }

    /// The row for `tool`.
    #[must_use]
    pub const fn support(&self, tool: HostedRetrievalTool) -> HostedRetrievalSupport {
        match tool {
            HostedRetrievalTool::WebSearch => self.web_search,
            HostedRetrievalTool::WebFetch => self.web_fetch,
            HostedRetrievalTool::XSearch => self.x_search,
        }
    }
}
