use std::path::Path;
use std::process::Stdio;

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
}

impl Executor {
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
        let mut command = command(cwd);
        if let Some(repo) = repository {
            command.env("GIT_DIR", &repo.git_dir);
            command.env("GIT_WORK_TREE", &repo.toplevel);
        }
        if let Some(attributes) = attributes {
            command.env("GIT_ATTR_SOURCE", attributes);
            command.arg(format!("--attr-source={attributes}"));
        }
        command.args(args);
        self.output(command, cap).await
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
        let group = Group(
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
                let (bytes, _, status) = tokio::try_join!(
                    Box::pin(drain(stdout, cap)),
                    Box::pin(drain(stderr, 8 * 1024)),
                    async { child.wait().await.map_err(|_| ChangesRefusal::Io) },
                )?;
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

fn command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    // Empty-then-curated also removes unknown future GIT_* steering variables.
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
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
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0)
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
        "submodule.recurse=false",
        "status.relativePaths=false",
    ] {
        command.args(["-c", setting]);
    }
    command
}
