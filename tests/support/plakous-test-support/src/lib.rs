//! Unpublished deterministic fixtures and conformance helpers for Plakous tests.
//!
//! Production crates must not depend on this package. Repository tests and
//! examples use it through dev-dependencies so test doubles do not become part
//! of the supported component or facade surface.

mod conformance;
mod model;
mod tool;

pub use conformance::{recover_model_operation, run_model_operation};
pub use model::{
    ExpectedRequest, ModelScenario, ScenarioBarrier, ScenarioStart, ScenarioStep, ScriptedModel,
    ScriptedRecoverySnapshotError, ScriptedRecoveryState,
};
pub use tool::{CapturedToolInvocation, ScriptedTool, ScriptedToolStep};
