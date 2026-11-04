//! Background reaping of leaders whose groups outlived their cleanup.

use std::time::Duration;

use super::super::{Mutex, lock};
use super::{GroupProbe, peek_leader_exit};

/// A leader the reaper owns until it reaps it.
///
/// The reaper holds the only handle, so nothing else can reap the leader:
/// its pid stays pinned to this process while unreaped, and the reap below
/// cannot take a reused pid's exit.
pub(in crate::environment::backend) trait OwnedLeader:
    Send
{
    /// Reaps the exited leader. True once it is reaped or cannot be waited.
    fn reap(&mut self) -> bool;
}

impl OwnedLeader for std::process::Child {
    fn reap(&mut self) -> bool {
        !matches!(self.try_wait(), Ok(None))
    }
}

impl OwnedLeader for Box<dyn portable_pty::Child + Send + Sync> {
    fn reap(&mut self) -> bool {
        !matches!(self.try_wait(), Ok(None))
    }
}

impl OwnedLeader for tokio::process::Child {
    fn reap(&mut self) -> bool {
        !matches!(self.try_wait(), Ok(None))
    }
}

struct Pending {
    leaders: Vec<(rustix::process::Pid, Box<dyn OwnedLeader>)>,
    running: bool,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending {
    leaders: Vec::new(),
    running: false,
});

/// Takes an unreaped leader whose group outlived its cleanup and reaps it in
/// the background once its group reads empty.
///
/// Reaping frees the PGID for reuse, so a leader whose group still has live
/// members stays unreaped, pinning it, and is retried about once a second.
/// Nothing is signalled here. Without this a long-lived host would keep one
/// zombie per failed cleanup.
pub(in crate::environment::backend) fn reap_when_group_empty(
    group: rustix::process::Pid,
    leader: Box<dyn OwnedLeader>,
) {
    let mut pending = lock(&PENDING);
    pending.leaders.push((group, leader));
    if pending.running {
        return;
    }
    let reaper = std::thread::Builder::new()
        .name(String::from("plak-reaper"))
        .spawn(reap_pending);
    // Without a reaper the leaders stay zombies that pin their PGIDs.
    pending.running = reaper.is_ok();
}

fn reap_pending() {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let leaders = std::mem::take(&mut lock(&PENDING).leaders);
        let unreaped = leaders
            .into_iter()
            .filter_map(|(group, mut leader)| {
                let reaped = match peek_leader_exit(group) {
                    Ok(Some(_)) => GroupProbe::new(group).members() == 0 && leader.reap(),
                    // No longer a child of this process: already gone.
                    Err(rustix::io::Errno::CHILD) => true,
                    // Still running, or nothing proves the group empty:
                    // retry on the next pass.
                    Ok(None) | Err(_) => false,
                };
                (!reaped).then_some((group, leader))
            })
            .collect::<Vec<_>>();
        let mut pending = lock(&PENDING);
        pending.leaders.extend(unreaped);
        if pending.leaders.is_empty() {
            pending.running = false;
            return;
        }
    }
}
