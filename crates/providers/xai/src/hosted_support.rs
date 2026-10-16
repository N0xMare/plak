//! Hosted retrieval support on the xAI routes. A row moves to `Supported`
//! only with a reviewed live qualification.

use plakous_model::{HostedRetrievalSupport, HostedRetrievalTable};

/// SuperGrok subscription route.
pub const SUPERGROK_HOSTED_RETRIEVAL: HostedRetrievalTable = HostedRetrievalTable::new(
    HostedRetrievalSupport::Supported,
    HostedRetrievalSupport::NOT_OFFERED,
    HostedRetrievalSupport::Supported,
);

/// xAI API route.
pub const XAI_API_HOSTED_RETRIEVAL: HostedRetrievalTable = HostedRetrievalTable::new(
    HostedRetrievalSupport::Supported,
    HostedRetrievalSupport::NOT_OFFERED,
    HostedRetrievalSupport::Supported,
);
