//! Hosted retrieval support on the OpenRouter route, every wire. A row moves
//! to `Supported` only with a reviewed live qualification.

use plakous_model::{HostedRetrievalSupport, HostedRetrievalTable};

/// OpenRouter route.
pub const OPENROUTER_HOSTED_RETRIEVAL: HostedRetrievalTable = HostedRetrievalTable::new(
    HostedRetrievalSupport::NOT_QUALIFIED,
    HostedRetrievalSupport::NOT_OFFERED,
    HostedRetrievalSupport::NOT_OFFERED,
);
