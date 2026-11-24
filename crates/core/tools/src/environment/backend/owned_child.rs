//! Exact wait ownership for host service and Changes children.

use std::collections::HashMap;
use std::future::Future as _;
use std::io;
use std::process::ExitStatus;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::task::Poll;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

use super::process::{GroupProbe, leader_exited};

struct Entry {
    child: tokio::process::Child,
    group: GroupProbe,
    retiring: bool,
}

#[derive(Default)]
struct Children {
    entries: HashMap<Pid, Entry>,
    sealed: bool,
    reaper_running: bool,
}

static CHILDREN: LazyLock<Mutex<Children>> = LazyLock::new(Mutex::default);

fn children() -> MutexGuard<'static, Children> {
    CHILDREN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) struct OwnedChild {
    group: Pid,
    joined: bool,
}

pub(super) struct Spawned {
    pub child: OwnedChild,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl OwnedChild {
    pub fn spawn(command: &mut Command) -> io::Result<Spawned> {
        let mut children = children();
        if children.sealed {
            return Err(io::Error::other(
                "host service and Changes spawning is closed",
            ));
        }
        let mut child = command.process_group(0).kill_on_drop(true).spawn()?;
        let group = child
            .id()
            .and_then(|id| Pid::from_raw(id.cast_signed()))
            .ok_or_else(|| io::Error::other("missing child process group"))?;
        let (stdin, stdout, stderr) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take());
        children.entries.insert(
            group,
            Entry {
                child,
                group: GroupProbe::new(group),
                retiring: false,
            },
        );
        Ok(Spawned {
            child: Self {
                group,
                joined: false,
            },
            stdin,
            stdout,
            stderr,
        })
    }

    pub const fn group(&self) -> Pid {
        self.group
    }

    pub fn signal(&self, signal: Signal) {
        let entries = children();
        if !self.joined
            && entries
                .entries
                .get(&self.group)
                .is_some_and(|entry| !entry.retiring)
        {
            let _ = kill_process_group(self.group, signal);
        }
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self.poll_wait(|child| match child.try_wait() {
            Ok(None) => Poll::Pending,
            Ok(Some(status)) => Poll::Ready(Ok(status)),
            Err(error) => Poll::Ready(Err(error)),
        }) {
            Poll::Ready(result) => result.map(Some),
            Poll::Pending => Ok(None),
        }
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        // Tokio's wait is cancellation safe and keeps its readiness state in
        // Child. Never hold the registry lock between polls.
        std::future::poll_fn(|cx| self.poll_wait(|child| std::pin::pin!(child.wait()).poll(cx)))
            .await
    }

    fn poll_wait(
        &mut self,
        poll: impl FnOnce(&mut tokio::process::Child) -> Poll<io::Result<ExitStatus>>,
    ) -> Poll<io::Result<ExitStatus>> {
        let mut children = children();
        let Some(entry) = children
            .entries
            .get_mut(&self.group)
            .filter(|entry| !entry.retiring && !self.joined)
        else {
            return Poll::Ready(Err(io::Error::other("host child is closed")));
        };
        let result = poll(&mut entry.child);
        if matches!(result, Poll::Ready(Ok(_))) {
            self.joined = true;
            children.entries.remove(&self.group);
        }
        result
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.joined {
            return;
        }
        let mut children = children();
        if let Some(entry) = children.entries.get_mut(&self.group) {
            retire(self.group, entry);
            if !children.sealed && !children.reaper_running {
                children.reaper_running = std::thread::Builder::new()
                    .name("plak-child-reaper".into())
                    .spawn(reap_retired)
                    .is_ok();
            }
        }
    }
}

fn retire(group: Pid, entry: &mut Entry) {
    if !entry.retiring {
        // The owned, unreaped leader still pins its PGID.
        let _ = kill_process_group(group, Signal::KILL);
        entry.retiring = true;
    }
}

fn reap(entries: &mut HashMap<Pid, Entry>) {
    entries.retain(|&group, entry| {
        !entry.retiring
            || !leader_exited(group)
            || entry.group.members() != 0
            || !matches!(entry.child.try_wait(), Ok(Some(_)))
    });
}

fn reap_retired() {
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let mut children = children();
        reap(&mut children.entries);
        if !children.entries.values().any(|entry| entry.retiring) {
            children.reaper_running = false;
            return;
        }
    }
}

fn seal() {
    let mut children = children();
    children.sealed = true;
    for (&group, entry) in &mut children.entries {
        retire(group, entry);
    }
}

fn poll_reap() -> usize {
    let mut children = children();
    reap(&mut children.entries);
    children.entries.len()
}

/// Product stop hooks. Do not call in a process that will continue serving.
#[doc(hidden)]
pub mod shutdown {
    /// Prevent more spawns and signal the child groups still owned.
    pub fn seal() {
        super::seal();
    }

    /// Reap exited children whose groups have emptied and count the rest.
    #[must_use]
    pub fn poll_reap() -> usize {
        super::poll_reap()
    }
}
