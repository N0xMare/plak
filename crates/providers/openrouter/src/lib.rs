//! Thin dynamic OpenRouter route, discovery, authorization, and protocol dialect data.
//!
//! The crate owns route, catalog, authorization, capability tables, and narrow
//! OpenRouter dialect facts. Family kernels own request lowering, attempts,
//! stream decoding, terminal projection, and artifact execution.
//!
//! Provider-native request bodies, authorization values, and response objects
//! remain behind this crate's transport and canonical projection boundary.
//!
//! ```compile_fail
//! use plakous_openrouter::OpenRouterPreparedRequest;
//!
//! fn leak_request(prepared: &OpenRouterPreparedRequest) {
//!     let _ = prepared.request();
//! }
//! ```
//!
//! ```compile_fail
//! use plakous_openrouter::OpenRouterResponse;
//!
//! fn leak_response(response: &OpenRouterResponse) {
//!     let _ = response.native();
//! }
//! ```
//!
#![forbid(unsafe_code)]
// Protocol docs name OpenRouter; attempt/stream skeletons exceed 100 lines;
// match arms intentionally share retry-never bodies; Result helpers are pure wire.
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::redundant_closure_for_method_calls,
    clippy::collapsible_match,
    clippy::collapsible_if,
    clippy::empty_line_after_outer_attr,
    clippy::empty_line_after_doc_comments,
    clippy::double_must_use,
    clippy::ignored_unit_patterns,
    clippy::unnested_or_patterns,
    clippy::unnecessary_wraps,
    clippy::unnecessary_lazy_evaluations,
    clippy::manual_let_else,
    clippy::items_after_statements,
    reason = "OpenRouter product name; dialect stream/attempt shape; shared retry-never arms"
)]

mod chat;
mod dialect;
mod discovery;
mod execution;
mod key_usage;
mod messages;
mod model;
mod route;

pub use chat::*;
pub use dialect::*;
pub use discovery::*;
pub use execution::*;
pub use key_usage::*;
pub use messages::*;
pub use model::*;
pub use route::*;
