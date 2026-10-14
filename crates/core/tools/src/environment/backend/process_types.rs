//! Validated process request and response values.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fmt, fs};

use super::{
    ExecutionBackendError, ExecutionBackendErrorKind,
    JOINED_PROCESS_SHELL_PROFILE_SEMANTIC_REVISION, MAX_ENVIRONMENT_OUTPUT_BYTES,
    MAX_EXECUTION_VALUE_BYTES, MAX_LITERAL_PROCESS_ARGUMENTS, MAX_LITERAL_PROCESS_STDIN_BYTES,
    ToolExecutorBindingIdentity, invalid_request, resource_error, validate_absolute_program,
    validate_literal, validate_process_value, validate_process_values, validate_relative_path,
};
use std::os::unix::fs::MetadataExt;

/// Opaque, allocation-local process-session identity. It is never an OS PID.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct JoinedProcessSessionId(pub(super) [u8; 16]);

impl JoinedProcessSessionId {
    pub(crate) const fn runtime_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Debug for JoinedProcessSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JoinedProcessSessionId(<opaque>)")
    }
}

/// Process terminal transport requested at start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JoinedProcessTerminalMode {
    /// Separate ordered stdout and stderr pipe records.
    Pipes,
    /// One Unix pseudo-terminal stream.
    UnixPty,
}

/// Explicit absolute shell profile selected outside model command text.
#[derive(Clone, Eq, PartialEq)]
pub struct JoinedProcessShellProfile {
    pub(super) executable: Arc<PathBuf>,
    pub(super) canonical_executable: Arc<PathBuf>,
    pub(super) executable_device: u64,
    pub(super) executable_inode: u64,
    pub(super) arguments: Arc<[String]>,
    pub(super) identity: JoinedProcessShellProfileIdentity,
}

/// Immutable identity of one exact enrolled shell filesystem object and argv profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinedProcessShellProfileIdentity {
    pub(super) semantic_revision: u32,
    pub(super) enrolled_executable: Arc<PathBuf>,
    pub(super) canonical_executable: Arc<PathBuf>,
    pub(super) executable_device: u64,
    pub(super) executable_inode: u64,
    pub(super) executable_mode_mask: u32,
    pub(super) arguments: Arc<[String]>,
    pub(super) binding_identity: ToolExecutorBindingIdentity,
}

impl JoinedProcessShellProfileIdentity {
    /// Returns the shell-profile semantic revision.
    #[must_use]
    pub const fn semantic_revision(&self) -> u32 {
        self.semantic_revision
    }
    /// Returns the enrolled absolute path before canonicalization.
    #[must_use]
    pub fn enrolled_executable(&self) -> &Path {
        &self.enrolled_executable
    }
    /// Returns the canonical enrolled executable path.
    #[must_use]
    pub fn canonical_executable(&self) -> &Path {
        &self.canonical_executable
    }
    /// Returns the exact fixed argv prefix.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
    /// Returns the content-addressed identity used by executable registration.
    #[must_use]
    pub const fn binding_identity(&self) -> ToolExecutorBindingIdentity {
        self.binding_identity
    }
    /// Returns the enrolled Unix device identity.
    #[must_use]
    pub const fn executable_device(&self) -> u64 {
        self.executable_device
    }
    /// Returns the enrolled Unix inode identity.
    #[must_use]
    pub const fn executable_inode(&self) -> u64 {
        self.executable_inode
    }
    /// Returns the Unix mode-bit mask that must remain nonzero.
    #[must_use]
    pub const fn executable_mode_mask(&self) -> u32 {
        self.executable_mode_mask
    }
}

impl JoinedProcessShellProfile {
    /// Creates an explicit shell executable and fixed arguments placed before
    /// the command string (for example `/bin/sh`, `["-c"]`).
    ///
    /// # Errors
    ///
    /// Rejects a non-absolute/non-UTF-8 executable or invalid fixed arguments.
    pub fn new(
        executable: impl Into<PathBuf>,
        arguments: impl IntoIterator<Item = String>,
    ) -> Result<Self, ExecutionBackendError> {
        let executable = executable.into();
        validate_absolute_program(&executable, "joined process shell must be absolute")?;
        let canonical_executable = fs::canonicalize(&executable)
            .map_err(|_| invalid_request("joined process shell could not be resolved"))?;
        let metadata = fs::metadata(&canonical_executable)
            .map_err(|_| invalid_request("joined process shell metadata is unavailable"))?;
        if !metadata.is_file() {
            return Err(invalid_request(
                "joined process shell must resolve to a regular file",
            ));
        }
        if metadata.mode() & 0o111 == 0 {
            return Err(invalid_request(
                "joined process shell is not marked executable",
            ));
        }
        let arguments = arguments.into_iter().collect::<Vec<_>>();
        validate_process_values(&arguments)?;
        let enrolled_text = executable
            .to_str()
            .ok_or_else(|| invalid_request("joined process shell path must be UTF-8"))?;
        let canonical_text = canonical_executable
            .to_str()
            .ok_or_else(|| invalid_request("canonical joined process shell path must be UTF-8"))?;
        let mut identity_parts = vec![
            b"joined-process-shell-profile".to_vec(),
            enrolled_text.as_bytes().to_vec(),
            canonical_text.as_bytes().to_vec(),
            b"regular-file-required".to_vec(),
        ];
        {
            identity_parts.push(metadata.dev().to_be_bytes().to_vec());
            identity_parts.push(metadata.ino().to_be_bytes().to_vec());
            identity_parts.push(0o111_u32.to_be_bytes().to_vec());
        }
        for argument in &arguments {
            identity_parts.push(b"argv".to_vec());
            identity_parts.push(argument.as_bytes().to_vec());
        }
        let borrowed = identity_parts.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let binding_identity = ToolExecutorBindingIdentity::new(
            JOINED_PROCESS_SHELL_PROFILE_SEMANTIC_REVISION,
            &borrowed,
        )
        .map_err(|_| {
            ExecutionBackendError::new(
                ExecutionBackendErrorKind::InvalidRequest,
                "joined process shell identity could not be computed",
            )
        })?;
        let enrolled_executable = Arc::new(executable);
        let canonical_executable = Arc::new(canonical_executable);
        let arguments: Arc<[String]> = arguments.into();
        let identity = JoinedProcessShellProfileIdentity {
            semantic_revision: JOINED_PROCESS_SHELL_PROFILE_SEMANTIC_REVISION,
            enrolled_executable: Arc::clone(&enrolled_executable),
            canonical_executable: Arc::clone(&canonical_executable),
            executable_device: metadata.dev(),
            executable_inode: metadata.ino(),
            executable_mode_mask: 0o111,
            arguments: Arc::clone(&arguments),
            binding_identity,
        };
        Ok(Self {
            executable: enrolled_executable,
            canonical_executable,
            executable_device: metadata.dev(),
            executable_inode: metadata.ino(),
            arguments,
            identity,
        })
    }

    /// Returns the exact absolute shell executable.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Returns fixed literal arguments preceding the command string.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    /// Returns the immutable exact shell enrollment identity.
    #[must_use]
    pub const fn identity(&self) -> &JoinedProcessShellProfileIdentity {
        &self.identity
    }

    pub(super) fn revalidated_executable(&self) -> Result<&Path, ExecutionBackendError> {
        let canonical = fs::canonicalize(self.executable.as_ref())
            .map_err(|_| invalid_request("joined process shell could not be revalidated"))?;
        if canonical != *self.canonical_executable {
            return Err(invalid_request(
                "joined process shell target changed after enrollment",
            ));
        }
        let metadata = fs::metadata(&canonical)
            .map_err(|_| invalid_request("joined process shell metadata changed"))?;
        if !metadata.is_file() {
            return Err(invalid_request(
                "joined process shell is no longer a regular file",
            ));
        }
        if metadata.dev() != self.executable_device
            || metadata.ino() != self.executable_inode
            || metadata.mode() & 0o111 == 0
        {
            return Err(invalid_request(
                "joined process shell object identity changed after enrollment",
            ));
        }
        // Execute the admitted target, not an alias that may have moved since
        // enrollment. The spawn boundary separately preserves the enrolled argv[0].
        Ok(self.canonical_executable.as_ref())
    }
}

impl fmt::Debug for JoinedProcessShellProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinedProcessShellProfile")
            .field("executable", &self.executable)
            .field("canonical_executable", &self.canonical_executable)
            .field("argument_count", &self.arguments.len())
            .finish_non_exhaustive()
    }
}

/// Closed process start command.
#[derive(Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum JoinedProcessCommand {
    /// An absolute executable and exact literal argv.
    Literal {
        /// Absolute executable.
        executable: Arc<PathBuf>,
        /// Exact argv values.
        arguments: Arc<[String]>,
    },
    /// A command string interpreted only by the named explicit shell profile.
    Shell {
        /// Selected shell profile.
        profile: JoinedProcessShellProfile,
        /// Exact command string passed as one final argument.
        command: Arc<str>,
    },
}

impl JoinedProcessCommand {
    /// Creates an exact literal command.
    ///
    /// # Errors
    ///
    /// Rejects a non-absolute/non-UTF-8 executable or invalid argv.
    pub fn literal(
        executable: impl Into<PathBuf>,
        arguments: impl IntoIterator<Item = String>,
    ) -> Result<Self, ExecutionBackendError> {
        let executable = executable.into();
        validate_absolute_program(&executable, "joined process executable must be absolute")?;
        let arguments = arguments.into_iter().collect::<Vec<_>>();
        validate_process_values(&arguments)?;
        Ok(Self::Literal {
            executable: Arc::new(executable),
            arguments: arguments.into(),
        })
    }

    /// Creates an explicit-shell command.
    ///
    /// # Errors
    ///
    /// Rejects an oversized command string or one containing NUL.
    pub fn shell(
        profile: JoinedProcessShellProfile,
        command: impl Into<String>,
    ) -> Result<Self, ExecutionBackendError> {
        let command = command.into();
        validate_process_value(&command)?;
        Ok(Self::Shell {
            profile,
            command: command.into(),
        })
    }
}

impl fmt::Debug for JoinedProcessCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal {
                executable,
                arguments,
            } => formatter
                .debug_struct("JoinedProcessCommand::Literal")
                .field("executable", executable)
                .field("argument_count", &arguments.len())
                .finish(),
            Self::Shell { profile, command } => formatter
                .debug_struct("JoinedProcessCommand::Shell")
                .field("profile", profile)
                .field("command_bytes", &command.len())
                .finish(),
        }
    }
}

/// Validated process-session start request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinedProcessStartRequest {
    pub(super) command: JoinedProcessCommand,
    pub(super) cwd: PathBuf,
    pub(super) stdin: Arc<[u8]>,
    pub(super) keep_stdin_open: bool,
    pub(super) terminal_mode: JoinedProcessTerminalMode,
    pub(super) output_spool_threshold: Option<usize>,
}

impl JoinedProcessStartRequest {
    /// Creates a bounded request. The working directory is workspace-relative.
    ///
    /// # Errors
    ///
    /// Rejects an invalid working directory or oversized initial input.
    pub fn new(
        command: JoinedProcessCommand,
        cwd: impl Into<PathBuf>,
        stdin: impl Into<Vec<u8>>,
        keep_stdin_open: bool,
        terminal_mode: JoinedProcessTerminalMode,
    ) -> Result<Self, ExecutionBackendError> {
        let cwd = cwd.into();
        validate_relative_path(&cwd, false)?;
        let stdin = stdin.into();
        if stdin.len() > MAX_LITERAL_PROCESS_STDIN_BYTES {
            return Err(resource_error(
                "joined process initial input exceeds its bound",
            ));
        }
        Ok(Self {
            command,
            cwd,
            stdin: stdin.into(),
            keep_stdin_open,
            terminal_mode,
            output_spool_threshold: None,
        })
    }
}

/// Disposition of stdin after a successful write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinedProcessInputDisposition {
    /// Retain the open pipe.
    KeepOpen,
    /// Close the pipe after writing all bytes.
    Close,
}

/// Stream associated with one ordered output record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinedProcessOutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
    /// Merged PTY output (reserved until PTY support is enrolled).
    Terminal,
}

/// One ordered, bounded output record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinedProcessOutputRecord {
    pub(super) sequence: u64,
    pub(super) cursor: u64,
    pub(super) stream: JoinedProcessOutputStream,
    pub(super) bytes: Arc<[u8]>,
}

impl JoinedProcessOutputRecord {
    /// Returns the session-local order witness.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Returns the byte cursor at the start of this record.
    #[must_use]
    pub const fn cursor(&self) -> JoinedProcessOutputCursor {
        JoinedProcessOutputCursor(self.cursor)
    }
    /// Returns the source stream.
    #[must_use]
    pub const fn stream(&self) -> JoinedProcessOutputStream {
        self.stream
    }
    /// Returns record bytes.
    #[must_use]
    pub const fn bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }
}

/// Opaque monotonic byte position in one joined process output stream.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct JoinedProcessOutputCursor(pub(super) u64);

impl JoinedProcessOutputCursor {
    /// Constructs an exact previously returned cursor.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the portable numeric projection of this cursor.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Current authoritative process status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JoinedProcessStatus {
    /// The owned group is still running.
    Running,
    /// The group exited and all owned resources joined.
    Exited(Option<i32>),
    /// Read-only durable row whose former owner left no exact terminal evidence.
    OutcomeUnknown,
    /// Explicit termination joined the owned resources.
    Terminated,
    /// Cancellation joined the owned resources.
    Cancelled,
    /// Execution elapsed its configured time bound and joined.
    TimedOut,
    /// Output exceeded a configured bound and cleanup joined.
    OutputLimit,
    /// I/O or wait failed after cleanup was attempted.
    Failed,
    /// The configured cleanup bound elapsed without proving group absence.
    CleanupUnresolved,
}

impl JoinedProcessStatus {
    /// Reports whether this service has no further live operations for the session.
    /// Read-only unknown snapshots are terminal without claiming a former process exit.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running | Self::CleanupUnresolved)
    }
}

/// Newly available ordered output and current status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinedProcessPollResult {
    pub(super) records: Arc<[JoinedProcessOutputRecord]>,
    pub(super) status: JoinedProcessStatus,
    pub(super) more_output: bool,
}

/// One non-consuming, cursor-bound read of joined process output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinedProcessCursorPollResult {
    pub(super) records: Arc<[JoinedProcessOutputRecord]>,
    pub(super) status: JoinedProcessStatus,
    pub(super) next_cursor: JoinedProcessOutputCursor,
    pub(super) more_output: bool,
    pub(super) skipped_bytes: u64,
}

impl JoinedProcessCursorPollResult {
    /// Output bytes between the requested cursor and the first returned byte
    /// that the session's bounded window no longer holds.
    #[must_use]
    pub const fn skipped_bytes(&self) -> u64 {
        self.skipped_bytes
    }
    /// Returns ordered bytes at or after the requested cursor.
    #[must_use]
    pub fn records(&self) -> &[JoinedProcessOutputRecord] {
        &self.records
    }
    /// Returns current process status without changing it.
    #[must_use]
    pub const fn status(&self) -> JoinedProcessStatus {
        self.status
    }
    /// Returns the cursor immediately after the returned bytes.
    #[must_use]
    pub const fn next_cursor(&self) -> JoinedProcessOutputCursor {
        self.next_cursor
    }
    /// Reports retained output after `next_cursor`.
    #[must_use]
    pub const fn more_output(&self) -> bool {
        self.more_output
    }
}

impl JoinedProcessPollResult {
    /// Returns ordered output not returned by an earlier poll.
    #[must_use]
    pub fn records(&self) -> &[JoinedProcessOutputRecord] {
        &self.records
    }
    /// Returns current process status.
    #[must_use]
    pub const fn status(&self) -> JoinedProcessStatus {
        self.status
    }
    /// Reports retained output beyond this poll's bounds.
    #[must_use]
    pub const fn more_output(&self) -> bool {
        self.more_output
    }
}

/// Declared process effects inspected by environment policy and audit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LiteralProcessIntent {
    pub(super) destructive: bool,
    pub(super) network: bool,
}

impl LiteralProcessIntent {
    /// Creates explicit intent flags.
    #[must_use]
    pub const fn new(destructive: bool, network: bool) -> Self {
        Self {
            destructive,
            network,
        }
    }

    /// Reports declared destructive behavior.
    #[must_use]
    pub const fn destructive(self) -> bool {
        self.destructive
    }

    /// Reports declared network behavior.
    #[must_use]
    pub const fn network(self) -> bool {
        self.network
    }
}

/// Literal executable plus separately retained argv/stdin data.
#[derive(Clone, Eq, PartialEq)]
pub struct LiteralProcessRequest {
    pub(super) program: Arc<str>,
    pub(super) arguments: Arc<[String]>,
    pub(super) cwd: PathBuf,
    pub(super) stdin: Arc<[u8]>,
    pub(super) intent: LiteralProcessIntent,
}

impl LiteralProcessRequest {
    /// Creates a bounded process request without a shell-string surface.
    ///
    /// # Errors
    ///
    /// Rejects empty/oversized values, too many arguments, traversal, NULs, and
    /// oversized stdin.
    pub fn new(
        program: impl Into<Arc<str>>,
        arguments: impl IntoIterator<Item = String>,
        cwd: impl Into<PathBuf>,
        stdin: impl Into<Vec<u8>>,
        intent: LiteralProcessIntent,
    ) -> Result<Self, ExecutionBackendError> {
        let program = program.into();
        validate_literal(&program, "literal process program is invalid")?;
        let arguments = arguments.into_iter().collect::<Vec<_>>();
        if arguments.len() > MAX_LITERAL_PROCESS_ARGUMENTS {
            return Err(invalid_request("literal process has too many arguments"));
        }
        let mut argument_bytes = 0_usize;
        for argument in &arguments {
            validate_literal(argument, "literal process argument is invalid")?;
            argument_bytes = argument_bytes.saturating_add(argument.len());
        }
        if argument_bytes > MAX_EXECUTION_VALUE_BYTES {
            return Err(invalid_request(
                "literal process arguments exceed their byte limit",
            ));
        }
        let cwd = cwd.into();
        validate_relative_path(&cwd, true)?;
        let stdin = stdin.into();
        if stdin.len() > MAX_LITERAL_PROCESS_STDIN_BYTES {
            return Err(invalid_request(
                "literal process stdin exceeds its byte limit",
            ));
        }
        Ok(Self {
            program,
            arguments: arguments.into(),
            cwd,
            stdin: stdin.into(),
            intent,
        })
    }

    /// Returns the exact program text.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Returns exact, separately retained arguments.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    /// Returns the relative process cwd.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Returns exact stdin bytes.
    #[must_use]
    pub fn stdin(&self) -> &[u8] {
        &self.stdin
    }

    /// Returns declared process intent.
    #[must_use]
    pub const fn intent(&self) -> LiteralProcessIntent {
        self.intent
    }
}

impl fmt::Debug for LiteralProcessRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiteralProcessRequest")
            .field("program_bytes", &self.program.len())
            .field("argument_count", &self.arguments.len())
            .field("cwd", &"<redacted>")
            .field("stdin_bytes", &self.stdin.len())
            .field("intent", &self.intent)
            .finish()
    }
}

/// Terminal literal process result with bounded raw streams.
#[derive(Clone, Eq, PartialEq)]
pub struct LiteralProcessResult {
    pub(super) exit_code: Option<i32>,
    pub(super) stdout: Arc<[u8]>,
    pub(super) stderr: Arc<[u8]>,
}

impl LiteralProcessResult {
    /// Creates a deterministic process result.
    ///
    /// # Errors
    ///
    /// Rejects streams beyond the exported hard environment maximum.
    pub fn new(
        exit_code: Option<i32>,
        stdout: impl Into<Vec<u8>>,
        stderr: impl Into<Vec<u8>>,
    ) -> Result<Self, ExecutionBackendError> {
        let stdout = stdout.into();
        let stderr = stderr.into();
        let hard = usize::try_from(MAX_ENVIRONMENT_OUTPUT_BYTES).unwrap_or(usize::MAX);
        if stdout.len() > hard || stderr.len() > hard {
            return Err(resource_error(
                "literal process result exceeds the hard output limit",
            ));
        }
        Ok(Self {
            exit_code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        })
    }

    /// Returns a normal exit code, or `None` for a simulated signal exit.
    #[must_use]
    pub const fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Returns bounded stdout bytes.
    #[must_use]
    pub const fn stdout(&self) -> &Arc<[u8]> {
        &self.stdout
    }

    /// Returns bounded stderr bytes.
    #[must_use]
    pub const fn stderr(&self) -> &Arc<[u8]> {
        &self.stderr
    }
}

impl fmt::Debug for LiteralProcessResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiteralProcessResult")
            .field("exit_code", &self.exit_code)
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .field("content", &"<redacted>")
            .finish()
    }
}
