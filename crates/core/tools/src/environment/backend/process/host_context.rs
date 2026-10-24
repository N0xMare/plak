//! Host process context, PATH, and literal execution.

use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;

use super::{
    Arc, AtomicBool, EnvironmentVariableName, EnvironmentVariableValue, ExecutionBackendError,
    ExecutionBackendFuture, ExecutionEnvironmentError, ExecutionEnvironmentErrorKind,
    ExecutionEnvironmentLimits, ExecutionOperationControl, ExecutionWorkspace, HostProcessOwner,
    Instant, LiteralProcessResult, MAX_EXECUTION_VALUE_BYTES, Path, PathBuf,
    PreparedLiteralProcess, cleanup_unresolved_error, fs, invalid_request, io_error,
    resolve_existing_no_symlink, supervise_host_process, workspace_error,
};

/// Minimal PATH used when the prepared request injects no PATH and the host
/// process itself has none.
const DEFAULT_HOST_PROCESS_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Captured process authority shared by literal, pipes and PTY execution.
pub(in crate::environment) struct HostProcessContext {
    pub(in crate::environment::backend) fallback_path: String,
    pub(in crate::environment::backend) home: Option<PathBuf>,
    pub(in crate::environment::backend) prefix: Option<(PathBuf, fs::File)>,
    pub(in crate::environment::backend) discovery_ceiling: Option<PathBuf>,
}

impl HostProcessContext {
    pub(in crate::environment) fn capture(
        workspace: &crate::WorkspacePolicy,
        options: super::super::super::HostProcessOptions,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let invalid = || {
            ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::Workspace,
                "required process PATH prefix is unavailable or outside the workspace",
            )
        };
        let prefix = options
            .required_path_prefix
            .map(|relative| {
                let path = workspace.root().join(relative);
                let canonical = fs::canonicalize(&path).map_err(|_| invalid())?;
                if canonical != path
                    || !canonical.starts_with(workspace.root())
                    || !canonical.is_dir()
                    || canonical.to_str().is_none_or(|path| path.contains(':'))
                {
                    return Err(invalid());
                }
                let directory = fs::File::open(&canonical).map_err(|_| invalid())?;
                Ok((canonical, directory))
            })
            .transpose()?;
        if options.discovery_ceiling
            && workspace
                .root()
                .parent()
                .and_then(Path::to_str)
                .is_none_or(|path| path.contains(':'))
        {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::Workspace,
                "repository discovery ceiling cannot be represented",
            ));
        }
        let fallback_path = std::env::var("PATH")
            .ok()
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= MAX_EXECUTION_VALUE_BYTES
                    && !value.contains('\0')
            })
            .unwrap_or_else(|| DEFAULT_HOST_PROCESS_PATH.to_owned());
        let home = captured_process_home(std::env::var_os("HOME").map(PathBuf::from));
        Ok(Self {
            fallback_path,
            home,
            prefix,
            discovery_ceiling: options.discovery_ceiling.then(|| {
                workspace
                    .root()
                    .parent()
                    .unwrap_or(workspace.root())
                    .to_path_buf()
            }),
        })
    }

    pub(super) fn required_prefix(&self) -> Result<Option<&Path>, ExecutionBackendError> {
        let Some((path, directory)) = &self.prefix else {
            return Ok(None);
        };
        let captured = directory
            .metadata()
            .map_err(|_| invalid_request("required process PATH prefix is unavailable"))?;
        let current = fs::symlink_metadata(path)
            .map_err(|_| invalid_request("required process PATH prefix is unavailable"))?;
        if !current.is_dir()
            || current.dev() != captured.dev()
            || current.ino() != captured.ino()
            || fs::canonicalize(path).ok().as_ref() != Some(path)
        {
            return Err(invalid_request(
                "required process PATH prefix identity changed",
            ));
        }
        Ok(Some(path))
    }
}

fn captured_process_home(home: Option<PathBuf>) -> Option<PathBuf> {
    home.filter(|home| home.is_absolute())
        .map(|home| fs::canonicalize(&home).unwrap_or(home))
        .filter(|home| home.is_dir() && home != Path::new("/"))
}

pub(in crate::environment::backend) fn host_process_path(
    process: &HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
) -> Result<String, ExecutionBackendError> {
    let base = environment
        .iter()
        .find(|(name, value)| name.as_str() == "PATH" && !value.0.is_empty())
        .map_or(process.fallback_path.as_str(), |(_, value)| {
            value.0.as_ref()
        });
    Ok(match process.required_prefix()? {
        Some(prefix) => format!("{}:{base}", prefix.display()),
        None => base.to_owned(),
    })
}

/// The `HOME` a host spawn should carry, or `None` when it must not be set.
///
/// A curated `HOME` in the descriptor's environment wins; the captured host
/// home is a runtime fallback only, so an `env_clear`ed child never inherits an
/// empty `HOME`. Bash reads `~/.profile` from the passwd entry whether or not
/// `HOME` is set, so an unset `HOME` breaks every `$HOME`-relative profile line
/// and every tool that resolves `$HOME` inside the model's shell.
pub(in crate::environment::backend) fn host_process_home<'context>(
    process: &'context HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
) -> Option<&'context Path> {
    if environment
        .iter()
        .any(|(name, value)| name.as_str() == "HOME" && !value.0.is_empty())
    {
        return None;
    }
    process.home.as_deref()
}

/// Restore the required prefix after a login shell initializes its own base PATH.
pub(super) fn restore_path_after_login(
    process: &HostProcessContext,
    command: &str,
) -> Result<String, ExecutionBackendError> {
    let mut wrapper = String::new();
    if let Some(prefix) = process.required_prefix()? {
        let prefix = prefix.display().to_string().replace('\'', "'\\''");
        let _ = writeln!(wrapper, "PATH='{prefix}':\"$PATH\"; export PATH");
    }
    if let Some(ceiling) = &process.discovery_ceiling {
        let ceiling = ceiling.display().to_string().replace('\'', "'\\''");
        let _ = writeln!(
            wrapper,
            "GIT_CEILING_DIRECTORIES='{ceiling}'; export GIT_CEILING_DIRECTORIES"
        );
    }
    wrapper.push_str(command);
    Ok(wrapper)
}

/// Resolves `program` to an absolute executable path.
///
/// Absolute paths are accepted as-is (existence is checked at spawn). Basename
/// programs are looked up on the prepared PATH or the default host PATH so
/// models can call `true` / `printf` without absolute paths. Relative paths
/// containing `/` remain rejected (no workspace-relative program resolution).
pub(in crate::environment::backend) fn resolve_host_process_program(
    program: &str,
    process: &HostProcessContext,
    environment: &[(EnvironmentVariableName, EnvironmentVariableValue)],
) -> Result<PathBuf, ExecutionBackendError> {
    let path = Path::new(program);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if program.contains('/') || program.contains('\\') {
        return Err(invalid_request(
            "host literal process program must be absolute or a PATH basename",
        ));
    }
    if program.is_empty() {
        return Err(invalid_request(
            "literal process program was not found on the host PATH",
        ));
    }
    let path_value = host_process_path(process, environment)?;
    for directory in path_value.split(':') {
        if directory.is_empty() {
            continue;
        }
        let candidate = Path::new(directory).join(program);
        // execvp semantics: a non-executable file of the same name earlier on
        // PATH (uv installs a sourceable `~/.local/bin/env`) does not shadow
        // the real program later on PATH.
        if candidate.is_file()
            && rustix::fs::access(&candidate, rustix::fs::Access::EXEC_OK).is_ok()
        {
            return Ok(candidate);
        }
    }
    Err(invalid_request(
        "literal process program was not found on the host PATH",
    ))
}

pub(in crate::environment::backend) fn execute_host_literal_process(
    workspace: ExecutionWorkspace,
    request: PreparedLiteralProcess,
    limits: ExecutionEnvironmentLimits,
    control: ExecutionOperationControl,
    process: Arc<HostProcessContext>,
) -> ExecutionBackendFuture<LiteralProcessResult> {
    Box::pin(async move {
        control.check_cancelled()?;
        let program = resolve_host_process_program(
            request.request.program(),
            &process,
            request.environment.as_ref(),
        )?;
        let resolved_cwd = resolve_existing_no_symlink(&workspace, request.request.cwd())?;
        if !resolved_cwd.is_dir() {
            return Err(workspace_error(
                "literal process cwd is not a contained directory",
            ));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name(String::from("plak-host-process"))
            .spawn(move || {
                let result = supervise_host_process(
                    &process,
                    resolved_cwd,
                    &program,
                    &request,
                    limits,
                    &control,
                    &thread_stop,
                );
                let _ = sender.send(result);
            })
            .map_err(|_| io_error("host process supervisor could not start"))?;
        let mut owner = HostProcessOwner {
            stop,
            thread: Some(thread),
        };
        let result = receiver
            .await
            .map_err(|_| io_error("host process supervisor stopped without a result"))?;
        if !owner.join_until(Instant::now() + limits.cleanup_duration())? {
            return Err(cleanup_unresolved_error());
        }
        result
    })
}
