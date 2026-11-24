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
    probe: Option<GroupProbe>,
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
                probe: Some(GroupProbe::new(group)),
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

fn reap() {
    let retiring: Vec<_> = children()
        .entries
        .iter()
        .filter_map(|(&group, entry)| entry.retiring.then_some(group))
        .collect();
    for group in retiring {
        let probe = {
            let mut children = children();
            children
                .entries
                .get_mut(&group)
                .filter(|entry| entry.retiring && entry.probe.is_some() && leader_exited(group))
                .and_then(|entry| entry.probe.take())
        };
        let Some(mut probe) = probe else {
            continue;
        };
        // Only the owner of this probe can wait the retiring leader. It
        // pins the PGID while a potentially slow snapshot runs off-lock.
        let empty = probe.members() == 0;
        let mut children = children();
        if let Some(entry) = children
            .entries
            .get_mut(&group)
            .filter(|entry| entry.retiring)
        {
            if empty && matches!(entry.child.try_wait(), Ok(Some(_))) {
                children.entries.remove(&group);
            } else {
                entry.probe = Some(probe);
            }
        }
    }
}

fn reap_retired() {
    loop {
        std::thread::sleep(Duration::from_millis(250));
        reap();
        let mut children = children();
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
    reap();
    children().entries.len()
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
