//! xAI Responses provider facts and SuperGrok subscription authentication.
//!
//! Inference uses the shared Responses-family host. This crate intentionally
//! keeps Platform API-key and subscription authority separate. Platform execution
//! requires explicit caller-declared context bounds.

#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    clippy::ignored_unit_patterns,
    clippy::match_same_arms,
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::unnecessary_wraps,
    reason = "provider product names, closed error mappings, and bounded auth/test flows"
)]

mod api_models;
#[cfg(feature = "native-auth")]
mod context;
mod dialect;
mod model;
#[cfg(feature = "native-auth")]
mod native_auth;
#[cfg(feature = "native-auth")]
mod oauth_pins;
mod platform;
mod platform_binding;
mod platform_model;
mod responses_dialect;
mod route;
mod subscription_usage;

pub use api_models::{
    XaiApiModel, XaiApiModelError, XaiApiModelStatus, XaiListedModel, accept_xai_listed_models,
    list_xai_api_models, xai_api_bundled_listing, xai_api_key_fingerprint, xai_api_models,
};
pub(crate) use api_models::{xai_platform_capacity_admitted, xai_platform_effort_admitted};
pub use dialect::*;
pub use model::*;
#[cfg(feature = "native-auth")]
pub use native_auth::*;
pub use platform::*;
pub use platform_binding::*;
pub use platform_model::*;
pub use responses_dialect::*;
pub use route::*;
pub use subscription_usage::*;
