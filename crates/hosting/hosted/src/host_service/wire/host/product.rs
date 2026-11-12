//! Product authority required by a multi-session wire host.

use super::{
    Arc, CliFailure, CliResult, Created, Creation, HostFrame, HostPrincipal, HostReadLimits,
    HostReadTask, HostRequestContext, NodeAddress, OpenedSession, PickerProgress,
    ProtocolExtension, RequestRefusal, SessionRow, StoredSessionView,
};

/// What a host of many sessions needs of its product's stores.
pub trait HostSessions<P: ProtocolExtension>: Send + Sync {
    /// Commits product stream visibility only after its addressed answer is written.
    fn host_response_sent(&self, _context: &HostRequestContext, _response: &P::HostResponse) {}
    /// One bounded product stream frame; control and attachment output drain first.
    fn host_stream(&self, _context: &HostRequestContext) -> Option<HostFrame<P>> {
        None
    }
    /// Wakes the sole connection writer when private stream output is available.
    fn host_stream_notify(
        &self,
        _context: &HostRequestContext,
    ) -> Option<Arc<tokio::sync::Notify>> {
        None
    }
    /// Deadline outcome selected before work starts. Mutations may already
    /// have taken effect when their response deadline expires.
    fn host_request_timeout(
        &self,
        _request: &P::HostRequest,
    ) -> Result<P::HostResponse, RequestRefusal> {
        Err(RequestRefusal::DeadlineExceeded)
    }
    /// Categorical audit hook for refusals that precede product decoding or
    /// execution. Implementations must not log the request body.
    fn host_request_refused(&self, _context: HostRequestContext, _reason: &RequestRefusal) {}
    /// Retires process-local operator authority when its authenticated
    /// connection leaves. It never stops or releases an agent session.
    fn host_disconnect(&self, _principal: &HostPrincipal, _connection_id: u64, _generation: u64) {}
    /// Independent operator pins, including lost-instance pins. `None` checks
    /// host idle eligibility; this hook must do no I/O.
    fn operator_terminals_blocked(&self, _session: Option<&str>) -> bool {
        false
    }
    /// One immutable policy used by both welcome and console enforcement.
    fn host_read_limits(&self) -> HostReadLimits;
    /// Principal-specific capabilities, built before the handshake from the
    /// same verified identity and limits later used by its connection.
    fn welcome_extension(
        &self,
        principal: &HostPrincipal,
        limits: &HostReadLimits,
    ) -> P::WelcomeExtension;
    /// Begin attachment-free product work. Provider I/O belongs to the
    /// product's executor; this future is only polled, never awaited by the
    /// connection's frame loop. Dropping it cancels this connection's wait.
    fn host_request(
        &self,
        _context: HostRequestContext,
        _request: P::HostRequest,
    ) -> HostReadTask<P> {
        Box::pin(async {
            Err(RequestRefusal::Unsupported {
                detail: "host product reads are unavailable".to_owned(),
            })
        })
    }
    /// Whether the full product id exists, without directory row limits.
    ///
    /// # Errors
    /// The product store could not be read.
    fn contains(&self, session: &str) -> Result<bool, String>;
    /// Decorate a saved runner identity without probing or changing its authority.
    fn runner_label(&self, id: &str) -> plakous_host_protocol::wire::RunnerLabel {
        plakous_host_protocol::wire::RunnerLabel {
            name: id.to_owned(),
            runner_id: id.to_owned(),
        }
    }

    /// The directory's rows as the stores show them, newest first: each one
    /// dormant, with the activity its durable facts give, and
    /// `hosted_elsewhere` only for a holder of its tree that is not proven
    /// gone.
    /// Apply workspace, pinned runner and archive visibility filters before
    /// the product's directory row limit. A nonlocal key is never host-canonicalized.
    ///
    /// # Errors
    ///
    /// The stores cannot be read.
    fn rows(
        &self,
        workspace: Option<&str>,
        runner: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<SessionRow>, String>;

    /// The head of the tree's root line, or `None`. A successor that kept
    /// its source's node supersedes it independently of directory ordering.
    ///
    /// # Errors
    ///
    /// The stores cannot be read.
    fn session_of(&self, node: &NodeAddress) -> Result<Option<String>, String>;

    /// Opens `session` on a host worker that releases it once idle.
    ///
    /// # Errors
    ///
    /// The session does not open.
    fn open(&self, session: &str) -> CliResult<OpenedSession<P>>;

    /// Carries the released session under its current root selection.
    ///
    /// # Errors
    /// The carry's validation or durable transition refused.
    fn rebind(&self, _session: &str, _cause: &str) -> CliResult<Created> {
        Err(CliFailure::new(
            "this launcher does not support binding repair",
        ))
    }

    /// Stops durable work without opening a dormant session.
    ///
    /// # Errors
    /// Authority, storage or unsettled runner effects refused the stop.
    fn stop_dormant(&self, _session: &str) -> CliResult<()> {
        Err(CliFailure::new("this launcher cannot stop dormant work"))
    }

    /// Read-only link or reconciliation debt after a durable dormant stop.
    fn runner_pending(&self, _session: &str) -> bool {
        false
    }

    /// Uncapped durable continuation and runner-stop debt check. Products
    /// must not use their presentation directory limit to prove idleness.
    /// Unavailable storage or runner readiness must return true.
    fn durable_idle_exit_blocked(&self) -> bool {
        true
    }

    /// The refusal of an attach whose session did not open with `failure`:
    /// `tree_hosted_elsewhere` while another live host process holds its
    /// tree, `session_unavailable` with why otherwise.
    fn refusal(&self, session: &str, failure: &CliFailure) -> RequestRefusal;

    /// Creates the session a `new_session` asks for, or finds the one the
    /// same `request_id` created.
    ///
    /// # Errors
    ///
    /// The request's refusal: nothing was created.
    fn create(&self, request: &Creation<'_>) -> Result<Created, RequestRefusal>;

    /// Read stored attach facts without opening a runtime or taking a lease.
    /// Unless `include_unarchived`, return none before projecting an
    /// unarchived session's transcript.
    ///
    /// # Errors
    /// Product metadata or transcript facts could not be read.
    fn stored_view(
        &self,
        _session: &str,
        _include_unarchived: bool,
    ) -> Result<Option<StoredSessionView<P>>, RequestRefusal> {
        Ok(None)
    }

    /// Read the bounded placement picker without opening a session.
    ///
    /// # Errors
    /// Typed validation, transport or product refusal.
    fn options(
        &self,
        _workspace: Option<&plakous_host_protocol::wire::WorkspaceProbe>,
        _progress: &mut dyn FnMut(PickerProgress) -> Result<(), String>,
    ) -> Result<plakous_host_protocol::wire::SessionOptions, RequestRefusal> {
        Err(product_service_missing("session_options"))
    }

    /// Commit or clear the title override without opening a runtime.
    ///
    /// # Errors
    /// Typed product validation or write refusal.
    fn rename(
        &self,
        _session: &str,
        _title: Option<&str>,
    ) -> Result<Option<String>, RequestRefusal> {
        Err(product_service_missing("rename_session"))
    }

    /// Commit archive state under the product admission barrier.
    ///
    /// # Errors
    /// Typed lifecycle or write refusal.
    fn archive(&self, _session: &str, _archived: bool) -> Result<(), RequestRefusal> {
        Err(product_service_missing("archive_session"))
    }

    /// Run the guarded product deletion workflow after host release.
    ///
    /// # Errors
    /// Typed deletion fences or infrastructure refusal.
    fn delete(&self, _session: &str, _abandon_children: bool) -> Result<(), RequestRefusal> {
        Err(product_service_missing("delete_session"))
    }
}

fn product_service_missing(operation: &str) -> RequestRefusal {
    RequestRefusal::Unsupported {
        detail: format!("{operation} product service is not installed"),
    }
}
