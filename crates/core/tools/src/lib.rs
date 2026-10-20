//! Tool execution, registration, policy, and scheduling for Plakous.
//!
//! Canonical model-facing definitions and result content remain owned by
//! `plakous-model`. This crate freezes executable registrations, validates strict
//! invocations, applies provider-neutral policy, and supplies owned execution
//! contexts to tools.

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("plakous-tools requires a unix target (spec 06 supported platforms)");

mod jobs;
pub use jobs::{JobConfig, JobHost, JobHostCapacity, JobOwner, JobStatus, JobStopReason, JobView};

mod admission;
mod capabilities;
mod coding_presentation;
mod composition_markers;
mod context;
mod durable;
mod environment;
mod error;
mod execution_value;
mod exposure;
mod image;
pub mod internal;
mod invocation;
mod namespace;
mod outcome;
mod pack;
mod pack_support;
mod policy;
mod prompt_digest;
mod registry;
mod retrieval;
mod schema;
mod standard_identity;

pub use admission::*;
pub use capabilities::*;
pub use coding_presentation::*;
pub use composition_markers::*;
pub use context::*;
pub use durable::*;
pub use environment::*;
pub use error::*;
pub use execution_value::*;
pub use exposure::*;
pub use image::*;
pub use invocation::*;
pub use namespace::*;
pub use outcome::*;
pub use pack::*;
pub use pack_support::*;
pub use policy::*;
pub use prompt_digest::*;
pub use registry::*;
pub use retrieval::*;
pub use schema::*;
pub use standard_identity::*;

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    use crate::{ToolContext, ToolExecutor, ToolFuture, ToolInvocation, ToolOutcome};

    pub(crate) struct FixedTool {
        outcome: ToolOutcome,
    }

    impl FixedTool {
        pub(crate) const fn new(outcome: ToolOutcome) -> Self {
            Self { outcome }
        }
    }

    impl ToolExecutor for FixedTool {
        fn execute(
            self: Arc<Self>,
            _invocation: ToolInvocation,
            _context: ToolContext,
        ) -> ToolFuture {
            let outcome = self.outcome.clone();
            Box::pin(async move { Ok(outcome) })
        }
    }
}
