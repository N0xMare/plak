//! The host-spawned service process and its owned process group.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt as _;

use super::super::process::{
    GroupProbe, HostProcessContext, host_process_home, host_process_path, leader_exited,
    reap_when_group_empty, resolve_host_process_program,
};
use super::super::workspace::verified_workspace_root;
use super::super::{
    ExecutionBackendFuture, ExecutionWorkspace, OperationContext, cleanup_unresolved_error,
    io_error,
};
use super::{
    ExecutionBackendError, ExecutionBackendErrorKind, PreparedServiceProcess,
    SERVICE_PROCESS_TERMINATE_GRACE, ServiceHandle, ServiceOwner, ServiceProcess,
    ServiceStderrTail, backend_error, cancelled_after_spawn,
};

/// Bound on joining the process group after SIGKILL.
const SERVICE_PROCESS_JOIN_BOUND: Duration = Duration::from_secs(2);
const SERVICE_PROCESS_JOIN_POLL: Duration = Duration::from_millis(5);

pub(in crate::environment::backend) struct HostService {
    private_cwd: Option<PrivateCwd>,
    /// Taken only by `Drop`, which hands an unjoined leader to the reaper.
    child: Option<tokio::process::Child>,
    process_group: rustix::process::Pid,
    stderr_reader: Option<tokio::task::JoinHandle<()>>,
    joined: bool,
}

impl HostService {
    pub(super) async fn terminate(mut self, grace: Duration) -> Result<(), ExecutionBackendError> {
        let _ =
            rustix::process::kill_process_group(self.process_group, rustix::process::Signal::TERM);
        let mut joined = self.join_until(Instant::now() + grace).await?;
        if !joined {
            let _ = rustix::process::kill_process_group(
                self.process_group,
                rustix::process::Signal::KILL,
            );
            joined = self
                .join_until(Instant::now() + SERVICE_PROCESS_JOIN_BOUND)
                .await?;
        }
        if !joined {
            return Err(cleanup_unresolved_error());
        }
        self.joined = true;
        if let Some(reader) = self.stderr_reader.take() {
            // The pipe closes once every group member is gone; a process that
            // left the group may still hold it, so the reader is bounded too.
            let abort = reader.abort_handle();
            if tokio::time::timeout(SERVICE_PROCESS_JOIN_BOUND, reader)
                .await
                .is_err()
            {
                abort.abort();
            }
        }
        Ok(())
    }

    /// Reaps the leader once its group is empty. Until then the unreaped
    /// leader pins the PGID, so the TERM and KILL that precede and follow this
    /// wait, and `Drop`'s KILL, never reach a reused group.
    async fn join_until(&mut self, deadline: Instant) -> Result<bool, ExecutionBackendError> {
        let mut group = GroupProbe::new(self.process_group);
        loop {
            if leader_exited(self.process_group)
                && group.members() == 0
                && let Some(child) = self.child.as_mut()
                && child
                    .try_wait()
                    .map_err(|_| io_error("service process wait failed"))?
                    .is_some()
            {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(SERVICE_PROCESS_JOIN_POLL).await;
        }
    }
}

impl Drop for HostService {
    fn drop(&mut self) {
        if !self.joined {
            // An unresolved group may still use its cwd. Retain the private
            // directory rather than delete live process state without proof.
            if let Some(directory) = self.private_cwd.take() {
                std::mem::forget(directory);
            }
            let _ = rustix::process::kill_process_group(
                self.process_group,
                rustix::process::Signal::KILL,
            );
            // Still unreaped, so the PGID stays pinned until the group empties.
            if let Some(child) = self.child.take() {
                reap_when_group_empty(self.process_group, Box::new(child));
            }
        }
        if let Some(reader) = self.stderr_reader.take() {
            reader.abort();
        }
    }
}

pub(in crate::environment::backend) fn spawn_host_service(
    workspace: ExecutionWorkspace,
    request: PreparedServiceProcess,
    control: OperationContext,
    process: Arc<HostProcessContext>,
) -> ExecutionBackendFuture<ServiceProcess> {
    Box::pin(async move {
        control
            .check_cancelled()
            .map_err(|error| error.with_proven_no_effect(true))?;
        verified_workspace_root(&workspace)?;
        let environment = request.child_environment();
        let private_cwd = workspace
            .private_service_home
            .as_ref()
            .map(|excluded| PrivateCwd::new(excluded, process.home.as_deref()))
            .transpose()?;
        let program =
            resolve_host_process_program(request.request.program(), &process, &environment)
                .map_err(|error| {
                    backend_error(
                        error.kind(),
                        "service process program is not absolute or was not found on the host PATH",
                    )
                })?;
        let mut command = tokio::process::Command::new(program);
        command
            .args(request.request.arguments())
            .current_dir(
                private_cwd
                    .as_ref()
                    .map_or(workspace.root(), |directory| directory.0.as_path()),
            )
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        for (name, value) in &environment {
            if name.as_str() != "PATH" {
                command.env(name.as_str(), value.0.as_ref());
            }
        }
        command.env("PATH", host_process_path(&process, &environment)?);
        if let Some(home) = host_process_home(&process, &environment) {
            command.env("HOME", home);
        }
        for (name, secret) in &request.request.secret_environment {
            command.env(name.as_str(), secret.expose());
        }
        if let Some(ceiling) = &process.discovery_ceiling {
            command.env("GIT_CEILING_DIRECTORIES", ceiling);
        }
        let mut child = command.spawn().map_err(|_| {
            backend_error(
                ExecutionBackendErrorKind::InvalidRequest,
                "service process spawn failed",
            )
        })?;
        let Some(process_group) = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
        else {
            let _ = child.start_kill();
            return Err(io_error("service process has no process group"));
        };
        let stderr_tail = Arc::new(ServiceStderrTail::new(request.request.stderr_tail_bytes));
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let mut host = HostService {
            private_cwd,
            child: Some(child),
            process_group,
            stderr_reader: None,
            joined: false,
        };
        let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
            host.terminate(SERVICE_PROCESS_TERMINATE_GRACE).await?;
            return Err(io_error("service process stdio was not captured"));
        };
        host.stderr_reader = Some(tokio::spawn(read_stderr_tail(
            stderr,
            Arc::clone(&stderr_tail),
        )));
        if control.cancellation().is_cancelled() {
            host.terminate(SERVICE_PROCESS_TERMINATE_GRACE).await?;
            return Err(cancelled_after_spawn());
        }
        Ok(ServiceProcess::new(
            Box::pin(stdin),
            Box::pin(stdout),
            ServiceHandle::new(ServiceOwner::Host(host), stderr_tail),
        ))
    })
}

struct PrivateCwd(std::path::PathBuf);

impl PrivateCwd {
    fn new(
        excluded: &std::path::Path,
        user_home: Option<&std::path::Path>,
    ) -> Result<Self, ExecutionBackendError> {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let excluded = excluded
            .canonicalize()
            .map_err(|_| io_error("service product home unavailable"))?;
        let base = std::env::temp_dir()
            .canonicalize()
            .map_err(|_| io_error("service temporary directory unavailable"))?;
        let user_home = user_home.and_then(|home| home.canonicalize().ok());
        for _ in 0..64 {
            let path = base.join(format!(
                ".plak-mcp-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    let directory = Self(path);
                    if directory.0.starts_with(&excluded)
                        || excluded.starts_with(&directory.0)
                        || user_home.as_ref().is_some_and(|home| home == &directory.0)
                    {
                        return Err(io_error("private service cwd overlaps protected home"));
                    }
                    if std::fs::metadata(&directory.0)
                        .map_err(|_| io_error("private service cwd unavailable"))?
                        .permissions()
                        .mode()
                        & 0o777
                        != 0o700
                    {
                        return Err(io_error("private service cwd is not private"));
                    }
                    return Ok(directory);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(io_error("private service cwd could not be created")),
            }
        }
        Err(io_error("private service cwd names exhausted"))
    }
}

impl Drop for PrivateCwd {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod private_cwd_tests {
    use super::PrivateCwd;

    #[test]
    fn each_service_gets_an_empty_private_directory_outside_protected_home() {
        use std::os::unix::fs::PermissionsExt as _;
        let product = tempfile::tempdir().unwrap();
        let first = PrivateCwd::new(product.path(), Some(product.path())).unwrap();
        let second = PrivateCwd::new(product.path(), Some(product.path())).unwrap();
        assert_ne!(first.0, second.0);
        assert_eq!(std::fs::read_dir(&first.0).unwrap().count(), 0);
        assert_eq!(
            std::fs::metadata(&first.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(!first.0.starts_with(product.path()));
        assert!(!product.path().starts_with(&first.0));
        assert_ne!(first.0, product.path());
        let path = first.0.clone();
        drop(first);
        assert!(!path.exists());
        assert!(second.0.is_dir());
    }
}

async fn read_stderr_tail(mut stderr: tokio::process::ChildStderr, tail: Arc<ServiceStderrTail>) {
    let mut buffer = [0_u8; 4096];
    while let Ok(read) = stderr.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        tail.push(&buffer[..read]);
    }
}
