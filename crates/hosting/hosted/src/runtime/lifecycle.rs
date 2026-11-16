//! Content-free host workflow, timing and resource observation vocabulary.

use super::Duration;

/// Wall-clock phases before the host loop opens the canonical session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReadyPathTiming {
    /// `tui_presentation_bundle` (persisted catalogs, no live GET).
    pub presentation_ms: u64,
    /// `build_host_config` including Codex/Grok attach catalog HTTP.
    pub resolve_ms: u64,
}

/// Content-free process resource snapshot for telemetry (Linux + macOS).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessResourceSnapshot {
    /// Current resident bytes, absent when the platform probe cannot supply them.
    pub rss_bytes: Option<u64>,
    /// Peak resident bytes observed by the OS, not a host-enforced memory limit.
    pub peak_rss_bytes: Option<u64>,
    /// Process user-mode CPU time accumulated at the snapshot.
    pub user_cpu: Duration,
    /// Process kernel CPU time accumulated at the snapshot.
    pub system_cpu: Duration,
    /// Live thread count, absent when unavailable.
    pub threads: Option<u64>,
    /// Open descriptor count, absent when unavailable.
    pub fds: Option<u64>,
}

/// Purpose of a runtime construction, not an independent execution owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeRole {
    /// Construct the session's ordinary root turn binding.
    Primary,
    /// Construct the one paid compaction binding for an admitted operation.
    Compact,
    /// Recompile an explicit successor binding without silently changing the source.
    Rebind,
}

/// Workflow identity attached to host lifecycle observations and test crash cuts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostWorkflow {
    /// Delivery of a queued root prompt to canonical admission.
    RootDelivery,
    /// Immediate operator send using the current turn's control path.
    SendNow,
    /// Construction and publication of a forked session.
    Fork,
    /// Explicit successor construction under a new bound tuple.
    Rebind,
    /// One paid compaction dispatch and its product publication.
    Compact,
    /// A switched-away attachment, after its runtime release.
    SourceRelease,
    /// A seeded create (a carry or an import): `Prepared` before its runtime
    /// create, `RuntimeReturned` after it and before the record save.
    SeededCreate,
}

/// Observable workflow boundary; observing it grants no mutation or replay authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostBoundary {
    /// Product preparation has completed before the canonical runtime call.
    Prepared,
    /// Canonical runtime work returned, before product-side publication.
    RuntimeReturned,
    /// Product projection or receipt was published for the workflow.
    ProductPublished,
    /// Product-side durable commit completed.
    ProductCommitted,
    /// The host has the workflow's authoritative terminal result ready.
    TerminalReady,
}
