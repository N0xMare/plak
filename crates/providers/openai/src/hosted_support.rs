//! Hosted retrieval support on the OpenAI routes. A row moves to
//! `Supported` only with a reviewed live qualification.

use plakous_model::{HostedRetrievalSupport, HostedRetrievalTable};

/// ChatGPT subscription (Codex Responses) route.
pub const CODEX_HOSTED_RETRIEVAL: HostedRetrievalTable = HostedRetrievalTable::new(
    HostedRetrievalSupport::Supported,
    HostedRetrievalSupport::NOT_OFFERED,
    HostedRetrievalSupport::NOT_OFFERED,
);

/// OpenAI API (Responses) route.
pub const OPENAI_API_HOSTED_RETRIEVAL: HostedRetrievalTable = HostedRetrievalTable::new(
    HostedRetrievalSupport::Supported,
    HostedRetrievalSupport::NOT_OFFERED,
    HostedRetrievalSupport::NOT_OFFERED,
);
