//! OpenRouter vocabulary with a feature-selected runtime implementation.
use crate::binding::{CompositionSelection, ResolvedProtocol};
use plakous::{
    agent::ReasoningEffort,
    model::OutputBoundDisposition,
    sessions::{SessionRuntimeBinding, SessionTargetBinding},
};
use std::{num::NonZeroU32, path::PathBuf, sync::Arc};
#[cfg(feature = "openrouter")]
#[path = "openrouter/available.rs"]
mod implementation;
#[cfg(not(feature = "openrouter"))]
#[path = "openrouter/unavailable.rs"]
mod implementation;
pub use implementation::*;
pub(crate) use implementation::{carry_input_window, construct_openrouter_runtime_with_fence};
/// Inputs for a newly discovered Custom/experimental OpenRouter Chat runtime.
pub struct ProductOpenRouterSpec {
    /// Explicit workspace authority of this node.
    pub runner_services: Option<plakous::runner::RunnerServices>,
    /// Exact local credential selection, independent of the gateway protocol.
    pub account: crate::account::AccountRef,
    /// Construction-bound process budgets.
    pub job_config: plakous::tools::JobConfig,
    /// The per-request output bound a session being reopened was created with.
    ///
    /// `None` for a new session, which takes the figure this route attests now.
    pub created_output_bound: Option<OutputBoundDisposition>,
    /// Optional product-supplied observation; never credential or dispatch authority.
    pub exchange_observer: crate::exchange_observation::Observer,
    /// The credentials this binding dispatches under, resolved by the product.
    pub credentials: Arc<dyn OpenRouterCredentials>,
    /// Where the evaluator workers a local composition dispatches into are
    /// installed. The binder asks through the composition's locator port;
    /// the product supplies installation paths.
    pub sidecars: Arc<dyn plakous::tools::SidecarLocator>,
    /// The product home whose instructions the profile enrolls, when there is
    /// one. The binder reads nothing else out of it.
    pub product_home: Option<PathBuf>,
    pub literal_model: String,
    pub effort: Option<ReasoningEffort>,
    pub composition: CompositionSelection,
    pub wire: ResolvedProtocol,
    pub workspace_root: PathBuf,
    pub cwd: PathBuf,
    pub plan_state_authority: String,
    pub opening_durable_session: bool,
    /// Spawn tier of this session's node; the root is `mutate`.
    pub occupancy_tier: crate::agent_tree_protocol::Tier,
    /// Product `compaction.enabled`; gates tool-group scheduled compact.
    pub scheduled_compaction: bool,
    /// Product `turn.max_model_rounds`: the round cap each root turn is admitted under.
    pub max_model_rounds: usize,
    pub topology_extensions: crate::tools::ProductToolExtensions,
    pub cached_evidence: Option<OpenRouterChatModelEvidence>,
    pub telemetry: Option<Arc<plakous::agent::TelemetryEmitter>>,
    /// A `spawn_agent` child in the parent's process; see
    /// `isolated_tool_scheduler`.
    pub spawned_child: bool,
}

/// Exact facade runtime plus its reconstruction-complete session target.
pub struct ProductOpenRouterRuntime {
    /// Set when this reopen kept a budget the route has since moved.
    pub output_bound_rederived: Option<OutputBoundRederived>,
    pub runtime: SessionRuntimeBinding,
    pub target: SessionTargetBinding,
    pub evidence: OpenRouterChatModelEvidence,
}

/// A reopened session's own output budget beside the one its route now attests.
///
/// The session keeps its own, because that figure reaches the configuration
/// identity and from there the persisted harness plan, tool projection target,
/// request prefix and admission evidence, none of which can be re-signed. The
/// operator is told what the route now offers and how to get it, rather than
/// being left on a budget that silently stopped matching the route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputBoundRederived {
    /// The figure this session was created with, and still uses.
    pub previous: Option<NonZeroU32>,
    /// The figure the route attests now.
    pub current: Option<NonZeroU32>,
}

/// Result of one on-demand OpenRouter inference-key allowance read.
#[derive(Clone, Debug)]
pub enum OpenRouterKeyUsageProbe {
    /// No OpenRouter key in the product secret store.
    NotConfigured,
    /// The authenticated read succeeded, with the product's safe key label.
    Ok(Box<OpenRouterKeyUsage>, String),
    /// A named failure carrying no secret material.
    Failed(String),
    /// A rejected key, retaining the source that must be repaired.
    Rejected {
        /// Neutral redaction-safe failure facts.
        reason: String,
        /// The selected key source.
        source: plakous_host_protocol::views::KeySource,
    },
}

/// Optional observation of one completed allowance GET, never admission authority.
pub trait KeyUsageObserver: Send {
    /// Called before the operation leaves its in-flight scope. Failures must
    /// not change the provider result or initiate another request.
    fn completed(&self, observation: &OpenRouterKeyUsageProbe);
}
