//! Hosted retrieval support on the xAI routes. A row moves to `Supported`
//! only with a reviewed live qualification.

use plakous_model::{HostedRetrievalSupport, HostedRetrievalTable};

/// SuperGrok subscription route.
pub const SUPERGROK_HOSTED_RETRIEVAL: HostedRetrievalTable = HostedRetrievalTable::new(
    HostedRetrievalSupport::NOT_QUALIFIED,
    HostedRetrievalSupport::NOT_OFFERED,
    HostedRetrievalSupport::NOT_QUALIFIED,
);

/// xAI API route.
pub const XAI_API_HOSTED_RETRIEVAL: HostedRetrievalTable = HostedRetrievalTable::new(
    HostedRetrievalSupport::NOT_QUALIFIED,
    HostedRetrievalSupport::NOT_OFFERED,
    HostedRetrievalSupport::NOT_QUALIFIED,
);
