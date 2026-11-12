use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};

use plakous_model::CancellationToken;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::Instant;

use super::ChangesRefusal;

pub(super) struct Output {
    pub success: bool,
    pub bytes: Vec<u8>,
}

#[derive(Clone)]
pub(super) struct Executor {
    pub deadline: Instant,
    pub cancellation: CancellationToken,
    path: std::ffi::OsString,
    program: Arc<OnceLock<Result<PathBuf, ChangesRefusal>>>,
}

impl Executor {
    pub fn new(deadline: Instant, cancellation: CancellationToken, path: &std::ffi::OsStr) -> Self {
        Self {
            deadline,
            cancellation,
            path: std::env::join_paths(
                std::env::split_paths(path).filter(|entry| entry.is_absolute()),
            )
            .unwrap_or_default(),
            program: Arc::default(),
        }
    }

    #[cfg(test)]
    pub fn test_program(self, program: PathBuf) -> Self {
        let _ = self.program.set(Ok(program));
        self
    }

    pub async fn resolve(
        &self,
        workspace: &Path,
        toplevel: Option<&Path>,
    ) -> Result<PathBuf, ChangesRefusal> {
        let roots = vec![
            workspace.to_path_buf(),
            toplevel.unwrap_or(workspace).to_path_buf(),
        ];
        let path = self.path.clone();
        let pinned = self.program.clone();
        super::repository::blocking(move || {
            let roots = roots
                .into_iter()
                .map(|root| {
                    root.canonicalize()
                        .map_err(|_| ChangesRefusal::UnsafeRepository)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let program = pinned
                .get_or_init(|| resolve_program(&path, &roots))
                .clone()?;
            if roots.iter().any(|root| program.starts_with(root)) {
                return Err(ChangesRefusal::UnsafeRepository);
            }
            Ok(program)
        })
        .await
    }

    pub fn check(&self) -> Result<(), ChangesRefusal> {
        if self.cancellation.is_cancelled() {
            Err(ChangesRefusal::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(ChangesRefusal::Deadline)
        } else {
            Ok(())
        }
    }

    pub async fn run(
        &self,
        cwd: &Path,
        repository: Option<&super::ChangesRepository>,
        attributes: Option<&str>,
        args: &[&str],
        cap: usize,
    ) -> Result<Output, ChangesRefusal> {
        self.check()?;
        let program = self
            .resolve(cwd, repository.map(|repo| repo.toplevel.as_path()))
            .await?;
        let mut command = command(&program, cwd, &self.path);
        if let Some(repo) = repository {
            command.env("GIT_DIR", &repo.git_dir);
            command.env("GIT_WORK_TREE", &repo.toplevel);
            command.env(
                "GIT_CEILING_DIRECTORIES",
                repo.workspace.parent().unwrap_or(&repo.workspace),
            );
        }
        if let Some(attributes) = attributes {
            command.env("GIT_ATTR_SOURCE", attributes);
            command.arg(format!("--attr-source={attributes}"));
        }
        command.args(args);
        self.output(command, cap).await
    }

    pub async fn probe_lazy_fetch(
        &self,
        cwd: &Path,
        object: &str,
        disabled: bool,
    ) -> Result<Output, ChangesRefusal> {
        let program = self.resolve(cwd, None).await?;
        let mut command = command(&program, cwd, &self.path);
        // Only this disposable capability fixture permits a local transport.
        // Ignoring GIT_NO_LAZY_FETCH would fetch the known advertised object.
        command.env("GIT_ALLOW_PROTOCOL", "file").args([
            "-c",
            "protocol.file.allow=always",
            "cat-file",
            "-e",
            object,
        ]);
        if !disabled {
            command.env_remove("GIT_NO_LAZY_FETCH");
        }
        self.output(command, 1024).await
    }

    pub(super) async fn output(
        &self,
        mut command: Command,
        cap: usize,
    ) -> Result<Output, ChangesRefusal> {
        self.check()?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|_| ChangesRefusal::VcsUnavailable)?;
        let mut group = Group(
            child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .and_then(rustix::process::Pid::from_raw),
        );
        let stdout = child.stdout.take().ok_or(ChangesRefusal::Io)?;
        let stderr = child.stderr.take().ok_or(ChangesRefusal::Io)?;
        let result = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(ChangesRefusal::Cancelled),
            () = tokio::time::sleep_until(self.deadline) => Err(ChangesRefusal::Deadline),
            output = async {
                // Keep the unreaped leader's PID reserved until all pipe holders
                // finish. Cancellation can still kill that exact process group.
                let (bytes, _) = tokio::try_join!(
                    Box::pin(drain(stdout, cap)),
                    Box::pin(drain(stderr, 8 * 1024)),
                )?;
                let status = child.wait().await.map_err(|_| ChangesRefusal::Io)?;
                group.0 = None;
                Ok(Output { success: status.success(), bytes })
            } => output,
        };
        // Group owns all exits, including a descendant holding a pipe open.
        // Dropping the future also kills the group without awaiting a pipe.
        drop(group);
        // kill_on_drop reaps through Tokio; never wait past the request budget
        // for a process stuck in an uninterruptible filesystem syscall.
        result
    }
}

struct Group(Option<rustix::process::Pid>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}

async fn drain(mut pipe: impl AsyncRead + Unpin, cap: usize) -> Result<Vec<u8>, ChangesRefusal> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = pipe
            .read(&mut chunk)
            .await
            .map_err(|_| ChangesRefusal::Io)?;
        if read == 0 {
            return Ok(bytes);
        }
        if read > cap.saturating_sub(bytes.len()) {
            return Err(ChangesRefusal::OutputLimit);
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

pub(super) fn command(program: &Path, cwd: &Path, path: &std::ffi::OsStr) -> Command {
    let mut command = Command::new(program);
    // Empty-then-curated also removes unknown future GIT_* steering variables.
    command.env_clear();
    command.env("PATH", path);
    command
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CEILING_DIRECTORIES", cwd.parent().unwrap_or(cwd))
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("GIT_NO_LAZY_FETCH", "1")
        // An empty allowlist also overrides protocol.<name>.allow=always.
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0)
        .arg("--no-pager")
        .arg("--no-optional-locks");
    for setting in [
        "core.fsmonitor=false",
        "core.hooksPath=/dev/null",
        "core.attributesFile=/dev/null",
        "credential.helper=",
        "protocol.allow=never",
        "core.quotePath=false",
        "core.untrackedCache=false",
        "core.ignoreStat=false",
        "core.trustctime=true",
        "diff.ignoreSubmodules=all",
        "diff.renames=false",
        "diff.autoRefreshIndex=false",
        "submodule.recurse=false",
        "status.relativePaths=false",
    ] {
        command.args(["-c", setting]);
    }
    command
}

fn resolve_program(path: &std::ffi::OsStr, roots: &[PathBuf]) -> Result<PathBuf, ChangesRefusal> {
    for directory in std::env::split_paths(path).filter(|entry| entry.is_absolute()) {
        let Ok(program) = directory.join("git").canonicalize() else {
            continue;
        };
        let metadata = std::fs::metadata(&program).map_err(|_| ChangesRefusal::VcsUnavailable)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            continue;
        }
        if roots.iter().any(|root| program.starts_with(root)) {
            return Err(ChangesRefusal::UnsafeRepository);
        }
        return Ok(program);
    }
    Err(ChangesRefusal::VcsUnavailable)
}
