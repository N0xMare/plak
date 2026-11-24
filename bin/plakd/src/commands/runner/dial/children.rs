//! Own the SSH and launcher helpers created by runner dialing, independently of Tokio runtimes.

use std::collections::BTreeMap;
use std::io;
use std::process::{self, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::process::{ChildStdin, ChildStdout};
use tokio::signal::unix::{SignalKind, signal};

const REAP_BOUND: Duration = Duration::from_secs(2);
const REAP_POLL: Duration = Duration::from_millis(5);

struct Children {
    sealed: bool,
    next: u64,
    active: BTreeMap<u64, process::Child>,
}

static CHILDREN: Mutex<Children> = Mutex::new(Children {
    sealed: false,
    next: 0,
    active: BTreeMap::new(),
});

fn children() -> MutexGuard<'static, Children> {
    CHILDREN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) type SshStream = tokio::io::Join<ChildStdout, ChildStdin>;

pub(super) struct SshChild {
    id: u64,
    watcher: Option<tokio::task::JoinHandle<()>>,
    join_on_drop: bool,
}

struct Spawned {
    child: SshChild,
    stdin: Option<process::ChildStdin>,
    stdout: Option<process::ChildStdout>,
}

fn spawn(command: &mut Command) -> io::Result<Spawned> {
    let mut children = children();
    if children.sealed {
        return Err(io::Error::other(
            "runner helper spawning is closed for host shutdown",
        ));
    }
    let mut child = command.spawn()?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let id = children.next;
    children.next += 1;
    children.active.insert(id, child);
    Ok(Spawned {
        child: SshChild {
            id,
            watcher: None,
            join_on_drop: true,
        },
        stdin,
        stdout,
    })
}

pub(super) fn bridge(command: &mut Command) -> io::Result<(SshChild, SshStream)> {
    let mut exited = signal(SignalKind::child())?;
    let Spawned {
        mut child,
        stdin,
        stdout,
    } = spawn(command.stdin(Stdio::piped()).stdout(Stdio::piped()))?;
    let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
        return Err(io::Error::other("missing runner SSH pipes"));
    };
    let stream = tokio::io::join(ChildStdout::from_std(stdout)?, ChildStdin::from_std(stdin)?);
    let id = child.id;
    child.watcher = Some(tokio::spawn(async move {
        let _ = wait(id, &mut exited).await;
    }));
    Ok((child, stream))
}

pub(super) async fn status(command: &mut Command) -> io::Result<ExitStatus> {
    command_status(command, true).await
}

pub(in crate::commands::runner) async fn launcher_status(
    command: &mut Command,
) -> io::Result<ExitStatus> {
    command_status(command, false).await
}

async fn command_status(command: &mut Command, join_on_drop: bool) -> io::Result<ExitStatus> {
    let mut exited = signal(SignalKind::child())?;
    let Spawned { mut child, .. } = spawn(command)?;
    child.join_on_drop = join_on_drop;
    wait(child.id, &mut exited).await
}

pub(in crate::commands::runner) async fn launcher_output(
    command: &mut Command,
) -> io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;

    let mut exited = signal(SignalKind::child())?;
    let Spawned {
        mut child, stdout, ..
    } = spawn(command.stdout(Stdio::piped()))?;
    child.join_on_drop = false;
    let mut stdout =
        ChildStdout::from_std(stdout.ok_or_else(|| io::Error::other("missing launcher stdout"))?)?;
    let mut output = Vec::new();
    stdout.read_to_end(&mut output).await?;
    wait(child.id, &mut exited).await?;
    Ok(output)
}

async fn wait(id: u64, exited: &mut tokio::signal::unix::Signal) -> io::Result<ExitStatus> {
    loop {
        {
            let mut children = children();
            let child = children
                .active
                .get_mut(&id)
                .ok_or_else(|| io::Error::other("runner SSH was closed"))?;
            // SIGCHLD is only a readiness hint. Each wait targets its own child.
            if let Some(status) = child.try_wait()? {
                children.active.remove(&id);
                return Ok(status);
            }
        }
        if exited.recv().await.is_none() {
            return Err(io::Error::other("runner SSH exit notification closed"));
        }
    }
}

impl Drop for SshChild {
    fn drop(&mut self) {
        if let Some(watcher) = &self.watcher {
            watcher.abort();
        }
        let mut ids = vec![self.id];
        kill(&mut ids);
        if self.join_on_drop {
            join(ids);
        } else if !ids.is_empty() {
            // A manager deadline must not also block its async caller for the
            // reap budget. The registry still owns any uncollected status.
            let _ = std::thread::Builder::new()
                .name("plak-launcher-reaper".into())
                .spawn(move || join(ids));
        }
    }
}

fn kill(ids: &mut Vec<u64>) {
    poll_reap(ids, |child| {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        match child.kill() {
            Ok(()) => Ok(false),
            Err(error) if child.try_wait()?.is_none() => Err(error),
            Err(_) => Ok(true),
        }
    });
}

fn join(mut ids: Vec<u64>) {
    let deadline = Instant::now() + REAP_BOUND;
    while !ids.is_empty() {
        poll_reap(&mut ids, |child| Ok(child.try_wait()?.is_some()));
        if ids.is_empty() {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::warn!(target: "plak.runner", unreaped = ids.len(), "runner_child_reap_timed_out");
            break;
        }
        std::thread::sleep(remaining.min(REAP_POLL));
    }
}

fn poll_reap(ids: &mut Vec<u64>, mut reap: impl FnMut(&mut process::Child) -> io::Result<bool>) {
    let mut children = children();
    ids.retain(|id| {
        let Some(child) = children.active.get_mut(id) else {
            return false;
        };
        match reap(child) {
            Ok(false) => true,
            Ok(true) => {
                children.active.remove(id);
                false
            }
            Err(error) => {
                tracing::warn!(target: "plak.runner", pid = child.id(), %error, "runner_child_reap_failed");
                false
            }
        }
    });
}

#[cfg(feature = "web")]
pub(crate) mod shutdown {
    pub(crate) fn seal() {
        // Seal spawning under the same lock as registration: exec retains the PID
        // even if a runtime or a connection owner has not finished dropping.
        let mut ids = {
            let mut children = super::children();
            children.sealed = true;
            children.active.keys().copied().collect()
        };
        super::kill(&mut ids);
    }

    pub(crate) fn poll_reap() -> usize {
        let mut children = super::children();
        children
            .active
            .retain(|_, child| !matches!(child.try_wait(), Ok(Some(_))));
        children.active.len()
    }
}

#[cfg(test)]
mod tests;
