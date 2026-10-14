//! Owned execution context, workspace containment, and bounded progress.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use plakous_model::{AttemptId, CallId, CancellationToken, Clock, RunId, Timestamp, TurnId};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::{
    EffectAuthorizationGrant, ExecutionEnvironment, ToolRuntimeError, ToolRuntimeErrorKind,
};

const MAX_PROGRESS_MESSAGE_BYTES: usize = 4_096;

/// Truthful execution-isolation classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IsolationLevel {
    /// Execution occurs with the embedding host process's authority.
    Host,
}

/// Canonical workspace path policy used by every filesystem-aware tool.
#[derive(Clone)]
pub struct WorkspacePolicy {
    root: Arc<PathBuf>,
    cwd: Arc<PathBuf>,
    isolation: IsolationLevel,
    /// Observational mutate-lease generation. Excluded from environment identity.
    generation: Arc<AtomicU64>,
    stamp_generation: bool,
}

impl WorkspacePolicy {
    /// Creates a policy after canonicalizing an existing workspace root.
    ///
    /// `cwd` must be a relative path beneath the root and must already exist.
    /// Host mode is reported by default; path containment does not confine process tools.
    ///
    /// # Errors
    ///
    /// Returns a workspace-policy error for I/O failure, traversal, or escape.
    pub fn host(root: impl AsRef<Path>, cwd: impl AsRef<Path>) -> Result<Self, ToolRuntimeError> {
        let root = canonicalize_safe(root.as_ref(), "workspace root could not be resolved")?;
        let relative_cwd = workspace_relative_suffix(&root, cwd.as_ref())?;
        let cwd = canonicalize_safe(
            &root.join(relative_cwd),
            "workspace current directory could not be resolved",
        )?;
        if !cwd.starts_with(&root) {
            return Err(workspace_error(
                "workspace current directory escapes the root",
            ));
        }
        Ok(Self {
            root: Arc::new(root),
            cwd: Arc::new(cwd),
            isolation: IsolationLevel::Host,
            generation: Arc::new(AtomicU64::new(0)),
            stamp_generation: false,
        })
    }

    /// Returns the canonical workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// Returns the canonical current directory.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        self.cwd.as_path()
    }

    /// Returns the truthful host isolation level.
    #[must_use]
    pub const fn isolation(&self) -> IsolationLevel {
        self.isolation
    }

    /// Share a live workspace generation counter for Read/Grep torn-read stamps.
    ///
    /// Not part of execution-environment identity. Host tree occupancy bumps the
    /// counter when a mutate lease publishes.
    #[must_use]
    pub fn with_generation_stamp(mut self, generation: Arc<AtomicU64>) -> Self {
        self.generation = generation;
        self.stamp_generation = true;
        self
    }

    /// Current workspace generation when this policy was constructed as a stamp.
    #[must_use]
    pub fn generation_stamp(&self) -> Option<u64> {
        self.stamp_generation
            .then(|| self.generation.load(Ordering::SeqCst))
    }

    /// Lowers a model-supplied path onto the workspace-relative suffix.
    ///
    /// An absolute path is accepted only when the canonical root is a
    /// lexical prefix of that path (the root itself or a descendant). Traversal
    /// and paths outside the root are refused. Internals stay relative.
    ///
    /// # Errors
    ///
    /// Returns a workspace-policy error when the path is not contained.
    pub fn relative_input(&self, path: impl AsRef<Path>) -> Result<PathBuf, ToolRuntimeError> {
        workspace_relative_suffix(self.root.as_path(), path.as_ref())
    }

    /// Resolves an existing path and rejects traversal or symlink escape.
    ///
    /// # Errors
    ///
    /// Returns a workspace-policy error when the path is invalid, absent, or
    /// resolves outside the canonical workspace root.
    pub fn resolve_existing(&self, path: impl AsRef<Path>) -> Result<PathBuf, ToolRuntimeError> {
        let relative = self.relative_input(path)?;
        let resolved = canonicalize_safe(
            &self.root.join(relative),
            "workspace path could not be resolved",
        )?;
        self.ensure_contained(resolved)
    }

    /// Resolves a possibly-new path by checking its nearest existing ancestor.
    ///
    /// This is lexical and symlink-aware at check time, but host mode does not
    /// claim race-free containment. Strong containment requires a future
    /// backend-owned isolation capability.
    ///
    /// # Errors
    ///
    /// Returns a workspace-policy error for traversal or symlink escape.
    pub fn resolve_for_write(&self, path: impl AsRef<Path>) -> Result<PathBuf, ToolRuntimeError> {
        let relative = self.relative_input(path)?;
        let candidate = self.root.join(relative);
        let mut ancestor = candidate.as_path();
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .ok_or_else(|| workspace_error("workspace path has no existing ancestor"))?;
        }
        let canonical_ancestor =
            canonicalize_safe(ancestor, "workspace path ancestor could not be resolved")?;
        if !canonical_ancestor.starts_with(self.root.as_path()) {
            return Err(workspace_error("workspace path escapes through a symlink"));
        }
        Ok(candidate)
    }

    fn ensure_contained(&self, resolved: PathBuf) -> Result<PathBuf, ToolRuntimeError> {
        if resolved.starts_with(self.root.as_path()) {
            Ok(resolved)
        } else {
            Err(workspace_error("workspace path resolves outside the root"))
        }
    }
}

impl fmt::Debug for WorkspacePolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspacePolicy")
            .field("root", &"<redacted>")
            .field("cwd", &"<redacted>")
            .field("isolation", &self.isolation)
            .finish_non_exhaustive()
    }
}

/// One bounded, ordered tool-progress update.
#[derive(Clone, Eq, PartialEq)]
pub struct ToolProgress {
    sequence: u64,
    call_id: CallId,
    timestamp: Timestamp,
    message: String,
    completed: Option<u64>,
    total: Option<u64>,
}

impl ToolProgress {
    /// Returns the monotonically increasing per-call sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the correlated canonical call ID.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns injected event time.
    #[must_use]
    pub const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Returns bounded model-independent progress text.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns completed units when reported.
    #[must_use]
    pub const fn completed(&self) -> Option<u64> {
        self.completed
    }

    /// Returns total units when reported.
    #[must_use]
    pub const fn total(&self) -> Option<u64> {
        self.total
    }
}

impl fmt::Debug for ToolProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolProgress")
            .field("sequence", &self.sequence)
            .field("call_id", &self.call_id)
            .field("timestamp", &self.timestamp)
            .field("message_bytes", &self.message.len())
            .field("message", &"<redacted>")
            .field("completed", &self.completed)
            .field("total", &self.total)
            .finish()
    }
}

#[derive(Debug)]
struct ProgressLifecycle {
    sequence: u64,
    closed: bool,
    sender: Option<mpsc::Sender<ToolProgress>>,
}

struct ProgressState {
    lifecycle: Mutex<ProgressLifecycle>,
    call_id: CallId,
    clock: Arc<dyn Clock>,
}

/// Cloneable non-blocking sink backed by a bounded channel.
#[derive(Clone)]
pub struct ProgressSink {
    state: Arc<ProgressState>,
}

impl fmt::Debug for ProgressSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let closed = match self.state.lifecycle.lock() {
            Ok(lifecycle) => lifecycle.closed,
            Err(poisoned) => poisoned.into_inner().closed,
        };
        formatter
            .debug_struct("ProgressSink")
            .field("call_id", &self.state.call_id)
            .field("closed", &closed)
            .finish_non_exhaustive()
    }
}

impl ProgressSink {
    /// Creates a bounded progress sink and its single consumer.
    ///
    /// # Errors
    ///
    /// Returns a resource-limit error when `capacity` is zero or exceeds the
    /// maximum supported by the asynchronous channel.
    pub fn channel(
        capacity: usize,
        call_id: CallId,
        clock: Arc<dyn Clock>,
    ) -> Result<(Self, ProgressReceiver), ToolRuntimeError> {
        if capacity == 0 {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::ResourceLimit,
                "progress channel capacity must be nonzero",
            ));
        }
        if capacity > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::ResourceLimit,
                "progress channel capacity exceeds supported bounds",
            ));
        }
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((
            Self {
                state: Arc::new(ProgressState {
                    lifecycle: Mutex::new(ProgressLifecycle {
                        sequence: 0,
                        closed: false,
                        sender: Some(sender),
                    }),
                    call_id,
                    clock,
                }),
            },
            ProgressReceiver {
                receiver: AsyncMutex::new(receiver),
            },
        ))
    }

    /// Emits progress without blocking execution.
    ///
    /// Saturated updates are coalesced by dropping the new update. Reporting
    /// after terminal close is suppressed.
    ///
    /// # Errors
    ///
    /// Returns a resource error for oversized text or inconsistent units.
    pub fn report(
        &self,
        message: impl Into<String>,
        completed: Option<u64>,
        total: Option<u64>,
    ) -> Result<(), ToolRuntimeError> {
        let message = message.into();
        if message.len() > MAX_PROGRESS_MESSAGE_BYTES {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::ResourceLimit,
                "tool progress message exceeds its byte limit",
            ));
        }
        if completed.zip(total).is_some_and(|(done, all)| done > all) {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "tool progress completed units exceed total units",
            ));
        }
        let mut lifecycle = match self.state.lifecycle.lock() {
            Ok(lifecycle) => lifecycle,
            Err(poisoned) => poisoned.into_inner(),
        };
        if lifecycle.closed {
            return Ok(());
        }
        let sequence = lifecycle.sequence;
        lifecycle.sequence = lifecycle.sequence.saturating_add(1);
        let progress = ToolProgress {
            sequence,
            call_id: self.state.call_id,
            timestamp: self.state.clock.now(),
            message,
            completed,
            total,
        };
        let Some(sender) = lifecycle.sender.as_ref() else {
            return Ok(());
        };
        match sender.try_send(progress) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                lifecycle.closed = true;
                lifecycle.sender = None;
                Ok(())
            }
        }
    }

    /// Suppresses all later updates after a terminal result.
    pub fn close(&self) {
        let mut lifecycle = match self.state.lifecycle.lock() {
            Ok(lifecycle) => lifecycle,
            Err(poisoned) => poisoned.into_inner(),
        };
        lifecycle.closed = true;
        lifecycle.sender = None;
    }

    /// Returns the canonical call identity bound to this sink.
    #[must_use]
    pub fn call_id(&self) -> CallId {
        self.state.call_id
    }
}

/// Single consumer for a bounded progress channel.
pub struct ProgressReceiver {
    receiver: AsyncMutex<mpsc::Receiver<ToolProgress>>,
}

impl fmt::Debug for ProgressReceiver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProgressReceiver")
            .finish_non_exhaustive()
    }
}

impl ProgressReceiver {
    /// Attempts to receive one update without blocking.
    #[must_use]
    pub fn try_recv(&self) -> Option<ToolProgress> {
        self.receiver.try_lock().ok()?.try_recv().ok()
    }

    /// Receives the next update asynchronously.
    ///
    /// Returns `None` after the sink is closed and all buffered updates have
    /// been received.
    pub async fn recv(&self) -> Option<ToolProgress> {
        self.receiver.lock().await.recv().await
    }
}

/// Boxed acknowledgement returned at the exact local-tool dispatch boundary.
/// Turn-scoped yield request, distinct from cancellation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolPreempt {
    /// No yield is requested.
    #[default]
    None,
    /// A queued instruction must reach the model.
    Steer,
    /// Explicit edge-triggered request to background in-flight processes.
    Background,
}

/// Owned, cloneable context supplied to an executing tool.
#[derive(Clone)]
pub struct ToolContext {
    run_id: RunId,
    turn_id: TurnId,
    attempt_id: AttemptId,
    call_id: CallId,
    cancellation: CancellationToken,
    preempt: tokio::sync::watch::Receiver<ToolPreempt>,
    deadline: Option<Timestamp>,
    clock: Arc<dyn Clock>,
    environment: Arc<ExecutionEnvironment>,
    authorization_grant: EffectAuthorizationGrant,
    progress: ProgressSink,
}

impl fmt::Debug for ToolContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolContext")
            .field("run_id", &self.run_id)
            .field("turn_id", &self.turn_id)
            .field("attempt_id", &self.attempt_id)
            .field("call_id", &self.call_id)
            .field("cancellation", &self.cancellation)
            .field("deadline", &self.deadline)
            .field("environment", &"<bound>")
            .field("authorization_grant", &self.authorization_grant)
            .field("progress", &self.progress)
            .finish_non_exhaustive()
    }
}

impl ToolContext {
    /// Creates a complete owned execution context from injected dependencies.
    #[allow(
        clippy::too_many_arguments,
        reason = "all invocation-bound context dependencies are explicit"
    )]
    #[must_use]
    pub(crate) fn authorized(
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: AttemptId,
        cancellation: CancellationToken,
        deadline: Option<Timestamp>,
        clock: Arc<dyn Clock>,
        environment: Arc<ExecutionEnvironment>,
        authorization_grant: EffectAuthorizationGrant,
        progress: ProgressSink,
    ) -> Self {
        Self {
            run_id,
            turn_id,
            attempt_id,
            call_id: progress.call_id(),
            cancellation,
            preempt: tokio::sync::watch::channel(ToolPreempt::None).1,
            deadline,
            clock,
            environment,
            authorization_grant,
            progress,
        }
    }

    /// Returns the run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the turn identity.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Returns the tool attempt identity.
    #[must_use]
    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }

    /// Returns the call identity bound to the progress path.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns cooperative cancellation.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Returns the turn's yield signal. The default receiver never fires.
    #[must_use]
    pub const fn preempt(&self) -> &tokio::sync::watch::Receiver<ToolPreempt> {
        &self.preempt
    }

    /// Binds the caller's turn-scoped yield signal without changing cancellation.
    #[must_use]
    pub fn with_preempt(mut self, preempt: tokio::sync::watch::Receiver<ToolPreempt>) -> Self {
        self.preempt = preempt;
        self
    }

    /// Returns the optional absolute deadline.
    #[must_use]
    pub const fn deadline(&self) -> Option<Timestamp> {
        self.deadline
    }

    /// Returns time from the injected clock.
    #[must_use]
    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Returns the canonical workspace policy.
    #[must_use]
    pub fn workspace(&self) -> &WorkspacePolicy {
        self.environment.workspace()
    }

    /// Returns the exact execution environment authorized for this invocation.
    #[must_use]
    pub const fn environment(&self) -> &Arc<ExecutionEnvironment> {
        &self.environment
    }

    /// Returns the policy or operator grant authorizing this exact invocation.
    #[must_use]
    pub const fn authorization_grant(&self) -> &EffectAuthorizationGrant {
        &self.authorization_grant
    }

    /// Returns the bounded progress sink.
    #[must_use]
    pub const fn progress(&self) -> &ProgressSink {
        &self.progress
    }

    /// Fails promptly when cancellation was already requested.
    ///
    /// # Errors
    ///
    /// Returns [`ToolRuntimeErrorKind::Cancelled`] after cancellation.
    pub fn check_cancelled(&self) -> Result<(), ToolRuntimeError> {
        if self.cancellation.is_cancelled() {
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                "tool execution was cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

/// The rule spec 04 §2.1 states, in the words the reader of a refusal needs.
///
/// The old text — "workspace paths must be relative and cannot traverse
/// parents" — described neither the rule nor what was wrong with the path. An
/// absolute path *is* accepted when it is inside the workspace root, so a
/// reader who took the message literally rewrote a path that was already fine,
/// and a reader who passed an outside path was never told that was the problem.
const PATH_RULE: &str = "file tools accept a path inside the workspace root: a relative path with no `..`, or an absolute path under the root";

/// What this containment does *not* cover (spec 04 §2.2).
///
/// A reader who is refused a path here and then watches `Bash` read the same
/// path concludes that one of the two is broken. Neither is: process tools
/// receive a bounded environment and a process group, not a fence, so an
/// arbitrary executable can reach whatever the host user can.
///
/// The line is about the *command*, not the tool. The shell tool's own
/// arguments are contained: spec 04 §2.1 gives a process-tool working directory
/// the per-component symlink check and a re-verification that it is still under
/// the root, and the execution backend refuses an outside path with this same
/// wording. Saying "the shell tool is not path-contained" would trade one
/// misleading sentence for another.
const SHELL_DIFFERS: &str =
    "a command the shell tool runs is not bound by this rule and can reach outside the root";

fn validate_relative(path: &Path) -> Result<&Path, ToolRuntimeError> {
    if path.as_os_str().is_empty() {
        return Ok(Path::new("."));
    }
    if path.is_absolute() || matches!(path.components().next(), Some(Component::Prefix(_))) {
        return Err(workspace_error(format!(
            "path is outside the workspace root; {PATH_RULE}. {SHELL_DIFFERS}"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(workspace_error(format!(
            "path leaves the workspace root with `..`; {PATH_RULE}. {SHELL_DIFFERS}"
        )));
    }
    Ok(path)
}

/// Lowers an input path onto a workspace-relative suffix under `root`.
///
/// Absolute paths are accepted only when the canonical root is a lexical
/// prefix of the path (the root itself or a descendant). `..` and other
/// roots stay refused. The returned path is then subject to
/// [`validate_relative`].
pub(crate) fn workspace_relative_suffix(
    root: &Path,
    path: &Path,
) -> Result<PathBuf, ToolRuntimeError> {
    let lowered = if path.is_absolute() {
        match path.strip_prefix(root) {
            Ok(suffix) if suffix.as_os_str().is_empty() => PathBuf::from("."),
            Ok(suffix) => suffix.to_path_buf(),
            // An absolute path under the root is accepted and lowered; only an
            // absolute path *elsewhere* is refused, and that is what it says.
            Err(_) => {
                return Err(workspace_error(format!(
                    "path is outside the workspace root `{}`; {PATH_RULE}. {SHELL_DIFFERS}",
                    root.display()
                )));
            }
        }
    } else {
        path.to_path_buf()
    };
    validate_relative(lowered.as_path()).map(Path::to_path_buf)
}

fn canonicalize_safe(path: &Path, context: &'static str) -> Result<PathBuf, ToolRuntimeError> {
    std::fs::canonicalize(path).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::WorkspacePolicy,
            context,
            &error,
        )
    })
}

fn workspace_error(message: impl AsRef<str>) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::WorkspacePolicy, message)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    /// The refusal a reader acts on states the rule and the exception.
    ///
    /// An outside path and a traversing path are different mistakes and must
    /// not share a sentence, and neither may claim that absolute paths are
    /// refused, because an absolute path under the root is accepted (spec 04
    /// §2.1). The shell's freedom from the same rule (§2.2) is stated rather
    /// than left to be discovered.
    #[test]
    fn a_path_refusal_states_the_rule_and_where_the_shell_differs()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        fs::write(workspace.path().join("inside.txt"), b"inside")?;
        let policy = WorkspacePolicy::host(workspace.path(), ".")?;
        let canonical = workspace.path().canonicalize()?;

        let outside = policy
            .relative_input("/etc/passwd")
            .expect_err("an absolute path outside the root is refused");
        assert_eq!(
            outside.message().as_str(),
            format!(
                "path is outside the workspace root `{}`; file tools accept a path inside the workspace root: a relative path with no `..`, or an absolute path under the root. a command the shell tool runs is not bound by this rule and can reach outside the root",
                canonical.display()
            )
        );

        let traversing = policy
            .relative_input("../outside")
            .expect_err("a traversing path is refused");
        assert_eq!(
            traversing.message().as_str(),
            "path leaves the workspace root with `..`; file tools accept a path inside the workspace root: a relative path with no `..`, or an absolute path under the root. a command the shell tool runs is not bound by this rule and can reach outside the root"
        );

        // The rule the message states is the rule the policy applies.
        assert_eq!(
            policy.relative_input(canonical.join("inside.txt"))?,
            PathBuf::from("inside.txt")
        );
        Ok(())
    }

    #[test]
    fn workspace_rejects_traversal_and_symlink_escape() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        let outside = tempdir()?;
        fs::write(workspace.path().join("inside.txt"), b"inside")?;
        let policy = WorkspacePolicy::host(workspace.path(), ".")?;
        // Resolution canonicalizes (macOS `/var` -> `/private/var`), so compare
        // against the canonical root rather than the raw tempdir path.
        assert!(
            policy
                .resolve_existing("inside.txt")?
                .starts_with(workspace.path().canonicalize()?)
        );
        assert!(policy.resolve_existing("../outside").is_err());

        let canonical = workspace.path().canonicalize()?;
        let under_root = canonical.join("inside.txt");
        assert!(
            policy
                .resolve_existing(&under_root)?
                .starts_with(&canonical)
        );
        assert_eq!(policy.relative_input(&canonical)?, PathBuf::from("."));
        assert_eq!(
            policy.relative_input(canonical.join("inside.txt"))?,
            PathBuf::from("inside.txt")
        );
        assert!(policy.relative_input("/etc/passwd").is_err());
        assert!(
            policy
                .relative_input(canonical.join("..").join("outside"))
                .is_err()
        );
        let mut root_slash = canonical.clone().into_os_string();
        root_slash.push("/");
        assert_eq!(policy.relative_input(root_slash)?, PathBuf::from("."));
        if let Some(parent) = canonical.parent() {
            let false_prefix = parent.join(format!(
                "{}-extra",
                canonical
                    .file_name()
                    .expect("canonical workspace root has a file name")
                    .to_string_lossy()
            ));
            assert!(policy.relative_input(false_prefix.join("foo")).is_err());
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), workspace.path().join("escape"))?;
            assert!(policy.resolve_existing("escape").is_err());
            assert!(policy.resolve_for_write("escape/new.txt").is_err());
        }
        Ok(())
    }

    #[test]
    fn workspace_paths_resolve_from_root_not_cwd() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        fs::write(workspace.path().join("Cargo.toml"), b"[package]\n")?;
        fs::create_dir(workspace.path().join("src"))?;
        fs::write(workspace.path().join("src/lib.rs"), b"pub fn lib() {}\n")?;
        let policy = WorkspacePolicy::host(workspace.path(), "src")?;
        assert!(
            policy
                .resolve_existing("Cargo.toml")?
                .ends_with("Cargo.toml")
        );
        assert!(policy.resolve_existing("lib.rs").is_err());
        assert!(
            policy
                .resolve_existing("src/lib.rs")?
                .ends_with("src/lib.rs")
        );
        Ok(())
    }

    #[test]
    fn generation_stamp_is_live_and_omitted_by_default() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        let policy = WorkspacePolicy::host(workspace.path(), ".")?;
        assert_eq!(policy.generation_stamp(), None);
        let counter = Arc::new(AtomicU64::new(3));
        let stamped =
            WorkspacePolicy::host(workspace.path(), ".")?.with_generation_stamp(counter.clone());
        assert_eq!(stamped.generation_stamp(), Some(3));
        counter.store(4, Ordering::SeqCst);
        assert_eq!(stamped.generation_stamp(), Some(4));
        Ok(())
    }
}
