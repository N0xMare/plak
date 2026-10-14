//! Thin OpenAI route, catalog, and authorization dialect for the shared Responses host.
//!
//! Codex subscription execution uses the shared Responses request, attempt,
//! stream, and projection path. OpenAI Platform uses the same shared host with
//! exact API-key authority and explicit caller context envelopes.

#![forbid(unsafe_code)]
// Protocol docs name OpenAI/Codex; attempt/stream skeletons exceed 100 lines;
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
    reason = "OpenAI product names; dialect stream/attempt shape; shared retry-never arms"
)]

mod api_models;
mod config;
mod error;
mod manifest;
mod model;
#[cfg(feature = "native-auth")]
mod native_auth;
#[cfg(feature = "native-auth")]
mod oauth_pins;
mod platform_local;
mod subscription_usage;

pub use api_models::{
    OpenAiApiModel, OpenAiApiModelStatus, OpenAiListedModel, accept_openai_listed_models,
    list_openai_api_models, openai_api_bundled_listing, openai_api_key_fingerprint,
    openai_api_models,
};
pub use config::*;
pub use error::*;
pub use manifest::{
    OPENAI_GPT_6_PLATFORM_PROFILE_ID, OPENAI_GPT_56_PLATFORM_PROFILE_ID,
    OPENAI_PLATFORM_MODEL_MANIFEST_EXPIRES_AT_UNIX_SECONDS,
    OPENAI_PLATFORM_MODEL_MANIFEST_REVIEWED_AT_UNIX_SECONDS,
    OPENAI_PLATFORM_MODEL_MANIFEST_REVISION,
};
pub use model::*;
#[cfg(feature = "native-auth")]
pub use native_auth::*;
pub use plakous_responses_dialect::ResponsesProviderConfigurationError;
pub use platform_local::*;
pub use subscription_usage::*;
