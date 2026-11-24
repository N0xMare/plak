//! The host can still join a worker after its cell or executor is gone.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::process::{Child, ChildStdin, ChildStdout, Command};

type SharedChild = Arc<tokio::sync::Mutex<Child>>;

struct Entry {
    child: SharedChild,
    dropped: bool,
}

struct Children {
    next: u64,
    entries: BTreeMap<u64, Entry>,
    sealed: bool,
    reaper_running: bool,
}

static CHILDREN: Mutex<Children> = Mutex::new(Children {
    next: 0,
    entries: BTreeMap::new(),
    sealed: false,
    reaper_running: false,
});

fn children() -> MutexGuard<'static, Children> {
    CHILDREN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) struct WorkerChild {
    id: u64,
    pub(super) child: SharedChild,
}

pub(super) struct Spawned {
    pub(super) child: WorkerChild,
    pub(super) pid: u32,
    pub(super) stdin: Option<ChildStdin>,
    pub(super) stdout: Option<ChildStdout>,
}

impl WorkerChild {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Spawned> {
        let mut children = children();
        if children.sealed {
            return Err(io::Error::other("cell worker spawning is closed"));
        }
        let mut child = command.kill_on_drop(true).spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("missing cell worker pid"))?;
        let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
        let child = Arc::new(tokio::sync::Mutex::new(child));
        let id = children.next;
        children.next += 1;
        children.entries.insert(
            id,
            Entry {
                child: Arc::clone(&child),
                dropped: false,
            },
        );
        Ok(Spawned {
            child: Self { id, child },
            pid,
            stdin,
            stdout,
        })
    }

    pub(super) fn joined(&self) {
        children().entries.remove(&self.id);
    }
}

impl Drop for WorkerChild {
    fn drop(&mut self) {
        let mut children = children();
        if let Some(entry) = children.entries.get_mut(&self.id) {
            entry.dropped = true;
            if kill_and_reap(entry) {
                children.entries.remove(&self.id);
            } else if !children.sealed && !children.reaper_running {
                children.reaper_running = std::thread::Builder::new()
                    .name("plak-cell-reaper".into())
                    .spawn(reap_dropped)
                    .is_ok();
            }
        }
    }
}

fn kill_and_reap(entry: &Entry) -> bool {
    let Ok(mut child) = entry.child.try_lock() else {
        return false;
    };
    match child.try_wait() {
        Ok(Some(_)) => return true,
        Err(error) if error.raw_os_error() == Some(nix::errno::Errno::ECHILD as i32) => {
            return true;
        }
        Ok(None) | Err(_) => {}
    }
    let _ = child.start_kill();
    false
}

fn reap_dropped() {
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let mut children = children();
        children
            .entries
            .retain(|_, entry| !entry.dropped || !kill_and_reap(entry));
        if !children.entries.values().any(|entry| entry.dropped) {
            children.reaper_running = false;
            return;
        }
    }
}

/// Product stop hooks. Do not call in a process that will continue serving.
#[doc(hidden)]
pub mod shutdown {
    /// Prevent new workers and begin stopping the ones still owned.
    pub fn seal() {
        let mut children = super::children();
        children.sealed = true;
        children
            .entries
            .retain(|_, entry| !super::kill_and_reap(entry));
    }

    /// Stop and reap remaining workers without waiting on their executors.
    #[must_use]
    pub fn poll_reap() -> usize {
        let mut children = super::children();
        children
            .entries
            .retain(|_, entry| !super::kill_and_reap(entry));
        children.entries.len()
    }
}
