//! Provider-neutral secure authentication foundations.
//!
//! The primary APIs provide secret containers, OAuth callback binding,
//! credential metadata, serialization boundaries, and durable private storage.
//! Core primitives deliberately know nothing about providers, endpoints, client
//! IDs, or models. Pure subscription OAuth pin tables live in
//! [`oauth_pins`] as data only. Pure shared OAuth workflow helpers live in
//! [`oauth_workflow`] (form encoding, authorize URL construction, device poll
//! math, callback query parse, field validation, access-token timing,
//! HTTP/1.1 request/response framing). The optional
//! `http-refresh` feature adds the claimed-refresh/revoke, browser-loopback,
//! and JWKS cache orchestrators ([`oauth_orchestrator`]) plus one provider-blind
//! bridge to the concrete Plakous HTTP transport so transmission facts cannot
//! be separated from durable refresh-claim transitions.
//!
//! Externally owned login-store readers (`external-compat`) were removed from
//! the product path: plak-owned OAuth is the only supported login surface.

#![forbid(unsafe_code)]

mod binding;
mod credential;
mod private_store;
mod secret;
/// In-process account/profile single-flight coordination.
///
/// OpenAI native JWKS refresh is the current consumer; xAI may adopt the same
/// primitive later. Keeping it shared is intentional even with one live caller.
mod single_flight;

/// Claimed OAuth refresh/revoke, browser loopback, and JWKS orchestrators
/// (feature `http-refresh`).
#[cfg(feature = "http-refresh")]
pub mod oauth_orchestrator;
/// Pure OAuth and compatibility pin tables for subscription dialects.
pub mod oauth_pins;
/// Shared pure OAuth workflow helpers for subscription dialects.
pub mod oauth_workflow;

#[doc(hidden)]
pub mod subscription_limit;

pub use binding::*;
pub use credential::*;
pub use private_store::*;
pub use secret::*;
pub use single_flight::*;
