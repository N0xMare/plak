//! Coalesced macOS group scans; unknown snapshots never prove group absence.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::super::{lock, set_nonblocking};

struct Snapshot {
    started: Instant,
    groups: Option<BTreeMap<i32, usize>>,
}

impl Snapshot {
    fn members(
        cached: &mut Option<Self>,
        group: i32,
        fresh_since: Instant,
        now: Instant,
        scan: impl FnOnce() -> Option<BTreeMap<i32, usize>>,
    ) -> Option<usize> {
        if cached.as_ref().is_none_or(|snapshot| {
            snapshot.started < fresh_since
                || now.saturating_duration_since(snapshot.started) >= Duration::from_millis(250)
        }) {
            let started = now;
            let groups = scan();
            *cached = Some(Self { started, groups });
        }
        let groups = cached.as_ref()?.groups.as_ref()?;
        Some(groups.get(&group).copied().unwrap_or(0))
    }
}

pub(super) fn members(group: rustix::process::Pid, fresh_since: Instant) -> Option<usize> {
    static SNAPSHOT: OnceLock<Mutex<Option<Snapshot>>> = OnceLock::new();
    // Hold the lock through collection so concurrent supervisors share one
    // complete scan rather than launching a ps process for every group. The
    // caller's freshness fence excludes snapshots from before group ownership
    // or the previous polling/termination phase (including PGID reuse).
    let mut snapshot = lock(SNAPSHOT.get_or_init(|| Mutex::new(None)));
    Snapshot::members(
        &mut snapshot,
        group.as_raw_pid(),
        fresh_since,
        Instant::now(),
        collect,
    )
}

fn parse(output: &[u8], visible_group: i32) -> Option<BTreeMap<i32, usize>> {
    let mut groups = BTreeMap::new();
    let mut visible = false;
    for line in std::str::from_utf8(output).ok()?.lines() {
        let mut fields = line.split_whitespace();
        let group = fields.next()?.parse::<i32>().ok()?;
        let state = fields.next()?;
        if fields.next().is_some() {
            return None;
        }
        visible |= group == visible_group;
        if !state.starts_with('Z') {
            *groups.entry(group).or_default() += 1;
        }
    }
    visible.then_some(groups)
}

fn collect() -> Option<BTreeMap<i32, usize>> {
    let mut child = Command::new("/bin/ps")
        .args(["-axo", "pgid=,stat="])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut output = Vec::new();
    let collected = (|| {
        let mut stdout = child.stdout.take()?;
        set_nonblocking(&stdout).ok()?;
        let deadline = Instant::now() + Duration::from_millis(100);
        let mut buffer = [0; 4096];
        loop {
            match std::io::Read::read(&mut stdout, &mut buffer) {
                Ok(0) => break,
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
            if output.len() > 1_048_576 || Instant::now() >= deadline {
                return None;
            }
        }
        loop {
            if let Some(status) = child.try_wait().ok()? {
                return status.success().then_some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })();
    if collected.is_none() {
        kill_and_reap(child);
        return None;
    }
    parse(&output, rustix::process::getpgrp().as_raw_pid())
}

fn kill_and_reap(mut child: std::process::Child) {
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_millis(10);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(_) => break,
        }
    }
    // A killed scanner may still be stuck in the kernel. Reap it off-lock so
    // no future membership probe waits indefinitely for this helper to exit.
    let _ = std::thread::Builder::new()
        .name("plak-ps-reap".to_owned())
        .spawn(move || {
            let _ = child.wait();
        });
}

#[cfg(test)]
mod cleanup_contention_tests {
    use super::*;

    #[test]
    fn shared_snapshot_counts_all_groups_and_refreshes_at_phase_fences() {
        let mut snapshot = None;
        let first_phase = Instant::now();
        assert_eq!(
            Snapshot::members(&mut snapshot, 11, first_phase, first_phase, || {
                parse(b"11 S\n11 Z\n22 R+\n", 11)
            }),
            Some(1)
        );
        assert_eq!(
            Snapshot::members(&mut snapshot, 22, first_phase, first_phase, || {
                panic!("duplicate scan")
            }),
            Some(1)
        );
        assert_eq!(
            Snapshot::members(&mut snapshot, 33, first_phase, first_phase, || {
                panic!("duplicate scan")
            }),
            Some(0)
        );
        let next_phase = first_phase + Duration::from_millis(1);
        assert_eq!(
            Snapshot::members(&mut snapshot, 11, next_phase, next_phase, || {
                parse(b"11 Z\n22 Z\n", 11)
            }),
            Some(0)
        );
        // Age forces a refresh independently of the caller's older fence.
        assert_eq!(
            Snapshot::members(
                &mut snapshot,
                22,
                first_phase,
                next_phase + Duration::from_millis(250),
                || parse(b"22 S\n", 22)
            ),
            Some(1)
        );
    }

    #[test]
    fn incomplete_shared_snapshot_never_proves_absence() {
        for output in [
            b"11 S\n22\n".as_slice(),
            b"invalid R\n",
            b"11 S extra\n",
            b"\xff",
        ] {
            assert!(parse(output, 11).is_none());
        }
        let mut snapshot = None;
        let phase = Instant::now();
        assert_eq!(
            Snapshot::members(&mut snapshot, 11, phase, phase, || None),
            None
        );
        assert_eq!(
            Snapshot::members(&mut snapshot, 22, phase, phase, || panic!("duplicate scan")),
            None
        );
        assert_eq!(
            Snapshot::members(
                &mut snapshot,
                22,
                phase + Duration::from_millis(1),
                phase + Duration::from_millis(1),
                || parse(b"22 S\n", 22)
            ),
            Some(1)
        );
    }

    #[test]
    fn snapshot_without_the_scanners_own_group_cannot_prove_absence() {
        assert!(parse(b"", 11).is_none());
        assert!(parse(b"22 S\n33 Z\n", 11).is_none());
        assert_eq!(parse(b"11 S\n22 Z\n", 11), Some(BTreeMap::from([(11, 1)])));
    }
}
