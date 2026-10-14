//! Provider-neutral inference and canonical tool-schema contracts for Plakous.
//!
//! The crate owns the validated semantic model shared by protocol adapters,
//! the object-safe model boundary, deterministic event assembly, and a
//! provider-neutral model contracts.

#![forbid(unsafe_code)]

mod configuration;
mod content;
mod context;
mod descriptor;
mod diagnostic;
mod digest;
mod event;
mod history;
mod identity;
mod model;
mod portable_schema_v2;
mod prompts;
mod request;
mod semantic_json;
mod subscription_usage;

pub use configuration::*;
pub use content::*;
pub use context::*;
pub use descriptor::*;
pub use diagnostic::*;
pub use digest::*;
pub use event::*;
pub use history::*;
pub use identity::*;
pub use model::*;
pub use portable_schema_v2::*;
pub use request::*;
pub use semantic_json::*;
pub use subscription_usage::*;
