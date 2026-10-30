//! Responses-family stream kernel.
//!
//! ## Ownership
//!
//! | Concern | Kernel | Dialect hooks |
//! | --- | --- | --- |
//! | SSE framing / blank-line termination | yes | map framing errors |
//! | Leading UTF-8 BOM | yes (profile) | — |
//! | Total-byte and event-count limits | yes | map limit errors |
//! | Sequence policy | yes | map sequence errors; receive validated sequence |
//! | `[DONE]` / typed-terminal finish | yes (profile) | map policy failures |
//! | Item/content/reasoning lifecycle | yes ([`ResponsesLifecycle`]) | parse JSON → natives; call lifecycle; map errors |
//! | JSON event shape parse | — | concrete family decoder |
//!
//! xAI, OpenRouter, and OpenAI Platform call [`ResponsesLifecycle`] for wire
//! correlation. OpenAI ModelEvent projection is dialect-local (the
//! projector in `plakous-openai`) — never hosted in this crate.

mod bom;
mod error;
mod framing;
mod kernel;
mod lifecycle;
mod limits;

pub use kernel::ResponsesStreamKernel;
pub use lifecycle::{
    LifecycleDialectPolicy, ReasoningLifecycleMode, ResponseModelBinding, ResponsesLifecycle,
    ResponsesLifecycleError, TerminalResponseInput,
};
pub use limits::{
    ResponsesDecodeLimitError, ResponsesDecodeLimits, ResponsesStreamLimitError,
    ResponsesStreamLimits,
};
