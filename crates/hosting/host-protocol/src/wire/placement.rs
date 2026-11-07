//! Placement and bounded creation-picker vocabulary for protocol 5.

use serde::{Deserialize, Serialize};

use super::RunnerLabel;
use crate::views::{ProfilesView, ReadyCatalog, TemplateRow};

/// Exact remote compatibility triple, independent of platform binary hash.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerVersion {
    /// Runner transport protocol, not client protocol.
    pub protocol: u32,
    /// Source revision.
    pub revision: String,
    /// Build flavor.
    pub flavor: String,
}

/// The phase whose bounded probe failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunnerPhase {
    /// Strict SSH connection.
    Ssh,
    /// Daemon startup/readiness.
    Daemon,
    /// Paired identity/version bind.
    Bind,
    /// Effective roots/defaults read.
    Options,
    /// Bounded repository discovery.
    Discovery,
    /// First-bind or reconnect recovery barrier.
    Reconcile,
    /// Established transport lost.
    LinkLost,
}

/// Latest readiness verdict, not continuous reachability.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerStatus {
    /// No probe completed in this host generation.
    Unknown,
    /// Bind and reconciliation completed.
    Ready,
    /// Placement cannot currently admit execution.
    Blocked {
        /// Typed remedy input.
        reason: PlacementReason,
    },
}

/// Why placement cannot be admitted; shared by statuses and refusals.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlacementReason {
    /// No configured alias still refers to the saved machine.
    RunnerNotConfigured {
        /// Identity that must be restored.
        saved_id: String,
        /// Stale requested alias.
        alias: Option<String>,
        /// Alias's replacement identity.
        current_id: Option<String>,
    },
    /// Pairing is absent or needs renewal.
    RunnerNotPaired,
    /// Responding machine differs from the pin.
    RunnerIdentityMismatch {
        /// Pinned identity.
        expected: String,
        /// Responding identity.
        actual: String,
    },
    /// Transport, daemon or recovery barrier unavailable.
    RunnerUnreachable {
        /// Failed phase.
        phase: RunnerPhase,
        /// Whether its deadline elapsed.
        timed_out: bool,
    },
    /// Remote binaries must match the exact triple.
    RunnerVersionMismatch {
        /// Required triple.
        expected: RunnerVersion,
        /// Advertised triple.
        actual: RunnerVersion,
    },
    /// Distinct identities on one machine overlap consented workspaces.
    RunnerRootsOverlap {
        /// Stable machine identity.
        machine_id: String,
        /// Conflicting placement identity.
        conflicting_runner: String,
        /// Authenticated operator-only sanitized path.
        path: String,
    },
    /// Runner-owned workspace validation failed before publication.
    WorkspaceRefused {
        /// Closed workspace verdict.
        kind: WorkspaceRefusalKind,
        /// Fresh spelling for `not_canonical`.
        canonical: Option<String>,
    },
}

/// Workspace checks are runner-owned, including canonical spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceRefusalKind {
    /// Not an absolute UTF-8 path without parent traversal.
    InvalidPath,
    /// Supplied bytes differ from canonical spelling.
    NotCanonical,
    /// No directory exists there.
    NotFound,
    /// Existing entry is not a directory.
    NotDirectory,
    /// Lexical or resolved path escapes roots.
    OutsideRoots,
    /// Directory is inside or contains the product home.
    ProductHome,
    /// Runner user cannot access it.
    PermissionDenied,
}

/// Binding resolution failures are not runner-health failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BindingRefusalKind {
    /// Profile absent or invalid.
    UnknownProfile,
    /// Root template absent.
    UnknownTemplate,
    /// Account/model not admitted.
    ModelUnavailable,
    /// Model cannot reproduce composition.
    IncompatibleComposition,
    /// Effective product configuration invalid.
    Configuration,
}

/// Explicit advanced root-only model override.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreationModel {
    /// Exact product account reference.
    pub account: String,
    /// Exact catalog model id.
    pub model: String,
}

/// Candidate workspace on one runner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceProbe {
    /// Selected pinned identity.
    pub runner: RunnerLabel,
    /// Absolute UTF-8 candidate.
    pub path: String,
}

/// Committed last use, bounded to 20 distinct workspaces per runner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecentWorkspace {
    /// Canonical runner-owned path.
    pub path: String,
    /// Unix seconds.
    pub updated_at: u64,
}

/// Repository HEAD states; unavailable is not detached.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryHead {
    /// Named branch with a commit.
    Branch {
        /// Git branch name.
        name: String,
    },
    /// Detached HEAD with a known commit.
    Detached,
    /// Named branch without a commit.
    Unborn {
        /// Git branch name.
        name: String,
    },
    /// Metadata could not safely be read.
    Unavailable,
}

/// Display-only bounded Git metadata; no remote URLs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryMetadata {
    /// Checkout basename.
    pub name: String,
    /// Tagged HEAD state.
    pub head: RepositoryHead,
    /// Full SHA; null for unborn/unavailable.
    pub commit: Option<String>,
}

/// Bounded discovery result on the selected runner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscoveredWorkspace {
    /// Canonical checkout path.
    pub path: String,
    /// None means no repository, not unavailable HEAD.
    pub repo: Option<RepositoryMetadata>,
}

/// One canonical runner entry in the creation picker.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerOptions {
    /// Canonical alias and pinned identity.
    pub runner: RunnerLabel,
    /// Other configured aliases for this same pinned identity, sorted.
    pub aliases: Vec<String>,
    /// Latest completed readiness verdict.
    pub status: RunnerStatus,
    /// Latest completed probe time; null while unknown.
    pub checked_at: Option<u64>,
    /// First failure in this uninterrupted blocked interval.
    pub blocked_since: Option<u64>,
    /// Effective roots; null for unrestricted local admission.
    pub roots: Option<Vec<String>>,
    /// First root, or local startup workspace.
    pub default_workspace: Option<String>,
    /// Newest 20 distinct committed workspaces.
    #[schemars(length(max = 20))]
    pub recent_workspaces: Vec<RecentWorkspace>,
    /// At most 200 checkout paths.
    #[schemars(length(max = 200))]
    pub discovered_workspaces: Vec<DiscoveredWorkspace>,
    /// Discovery reached its bounded limit.
    pub discovery_truncated: bool,
}

/// Optional workspace-specific validation and choices.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum WorkspaceOptions {
    /// Validated; local trusted overlays may specialize choices.
    Valid {
        /// Exact spelling required on create.
        canonical_path: String,
        /// Shared ungated profile summaries.
        profiles: ProfilesView,
        /// Advanced root-only choices.
        templates: Vec<TemplateRow>,
        /// Host-resolved catalog.
        models: ReadyCatalog,
    },
    /// Typed runner-owned refusal.
    Refused {
        /// Validation verdict.
        refusal: PlacementReason,
    },
}

/// Final bounded picker response after request-scoped progress.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionOptions {
    /// One canonical entry per pinned id.
    pub runners: Vec<RunnerOptions>,
    /// Host-resolved Ready catalog.
    pub models: ReadyCatalog,
    /// Shared ungated creation summaries.
    pub profiles: ProfilesView,
    /// Advanced root-only selections.
    pub templates: Vec<TemplateRow>,
    /// Optional candidate-path verdict.
    pub workspace: Option<WorkspaceOptions>,
}
