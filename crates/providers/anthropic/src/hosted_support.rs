//! Hosted retrieval support on the Anthropic routes. A row moves to
//! `Supported` only with a reviewed live qualification.

use plakous_model::{HostedRetrievalSupport, HostedRetrievalTable};

/// Claude subscription (OAuth Messages) route.
pub const CLAUDE_OAUTH_HOSTED_RETRIEVAL: HostedRetrievalTable = HostedRetrievalTable::new(
    HostedRetrievalSupport::NOT_QUALIFIED,
    HostedRetrievalSupport::NOT_QUALIFIED,
    HostedRetrievalSupport::NOT_OFFERED,
);

/// Anthropic API (Messages) route.
pub const ANTHROPIC_API_HOSTED_RETRIEVAL: HostedRetrievalTable = HostedRetrievalTable::new(
    HostedRetrievalSupport::NOT_QUALIFIED,
    HostedRetrievalSupport::NOT_QUALIFIED,
    HostedRetrievalSupport::NOT_OFFERED,
);
