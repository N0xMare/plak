//! One normalization from every protocol family's isolated side request.

use plakous_exchange::side_request;
use plakous_tools::{
    RetrievalError, RetrievalErrorKind, RetrievalOutcome, RetrievalSource, RetrievalUsage,
};

pub(crate) fn outcome(value: side_request::RetrievalOutcome) -> RetrievalOutcome {
    RetrievalOutcome {
        text: value.text,
        sources: value
            .sources
            .into_iter()
            .map(|source| RetrievalSource {
                url: source.url,
                title: source.title,
            })
            .collect(),
        usage: RetrievalUsage {
            input_tokens: value.usage.input_tokens,
            output_tokens: value.usage.output_tokens,
            hosted_requests: value.usage.hosted_requests,
        },
    }
}

pub(crate) fn error_of(error: &side_request::RetrievalError) -> RetrievalError {
    use side_request::RetrievalErrorKind as Kind;
    let kind = match error.kind() {
        Kind::InvalidRequest => RetrievalErrorKind::InvalidRequest,
        Kind::Unauthorized => RetrievalErrorKind::Unauthorized,
        Kind::Rejected => RetrievalErrorKind::Rejected,
        Kind::Cancelled => RetrievalErrorKind::Cancelled,
        Kind::InvalidResponse => RetrievalErrorKind::InvalidResponse,
        _ => RetrievalErrorKind::Unavailable,
    };
    RetrievalError::new(kind, error.message())
}
