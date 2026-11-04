//! Coalesced process-group scans; unknown snapshots never prove group absence.

use std::collections::BTreeMap;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::super::lock;
#[cfg(target_os = "macos")]
use super::super::set_nonblocking;

struct Snapshot {
    started: Instant,
    groups: Option<BTreeMap<i32, usize>>,
}

impl Snapshot {
    fn count(&self, group: i32) -> Option<usize> {
        self.groups
            .as_ref()
            .map(|groups| groups.get(&group).copied().unwrap_or(0))
    }

    fn serves(&self, fresh_since: Instant, now: Instant) -> bool {
        self.started >= fresh_since
            && now.saturating_duration_since(self.started) < Duration::from_millis(250)
    }
}

/// The two most recent shared scans.
#[derive(Default)]
struct Snapshots {
    latest: Option<Snapshot>,
    earlier: Option<Snapshot>,
}

impl Snapshots {
    /// The group's live-member count, or `None` when the scan is unknown.
    ///
    /// With `confirm_empty`, an empty count holds only when the scan before
    /// it, also begun after `fresh_since`, agrees; otherwise one more scan
    /// decides. A `/proc` readdir is not a point-in-time snapshot: a member
    /// forked below its cursor (after a pid wrap) by one that then exits is
    /// missed. Keeping the previous scan lets concurrent probes share the
    /// confirmation instead of each forcing its own.
    fn members(
        &mut self,
        group: i32,
        fresh_since: Instant,
        mut now: impl FnMut() -> Instant,
        confirm_empty: bool,
        mut scan: impl FnMut() -> Option<BTreeMap<i32, usize>>,
    ) -> Option<usize> {
        let checked = now();
        if !self
            .latest
            .as_ref()
            .is_some_and(|snapshot| snapshot.serves(fresh_since, checked))
        {
            self.rescan(checked, &mut scan);
        }
        let count = self.latest.as_ref()?.count(group);
        if !confirm_empty
            || count != Some(0)
            || self.earlier.as_ref().is_some_and(|snapshot| {
                snapshot.serves(fresh_since, checked) && snapshot.count(group) == Some(0)
            })
        {
            return count;
        }
        self.rescan(now(), &mut scan);
        self.latest.as_ref()?.count(group)
    }

    fn rescan(
        &mut self,
        started: Instant,
        scan: &mut impl FnMut() -> Option<BTreeMap<i32, usize>>,
    ) {
        let groups = scan();
        self.earlier = self.latest.replace(Snapshot { started, groups });
    }
}

pub(super) fn members(group: rustix::process::Pid, fresh_since: Instant) -> Option<usize> {
    static SNAPSHOTS: OnceLock<Mutex<Snapshots>> = OnceLock::new();
    // Hold the lock through collection so concurrent supervisors share one
    // complete scan of the host's processes rather than one per group: a scan
    // costs time in the host's process count, and per-group scans run in
    // parallel contend in the kernel. The caller's freshness fence excludes
    // snapshots from before group ownership or the previous
    // polling/termination phase (including PGID reuse).
    shared_members(
        SNAPSHOTS.get_or_init(Mutex::default),
        group.as_raw_pid(),
        fresh_since,
        live_groups,
    )
}

/// One probe of the shared snapshots. Only Linux confirms an empty count: its
/// `/proc` readdir is not a point-in-time snapshot, unlike a `ps` run.
fn shared_members(
    snapshots: &Mutex<Snapshots>,
    group: i32,
    fresh_since: Instant,
    scan: impl FnMut() -> Option<BTreeMap<i32, usize>>,
) -> Option<usize> {
    lock(snapshots).members(
        group,
        fresh_since,
        Instant::now,
        cfg!(target_os = "linux"),
        scan,
    )
}

/// Live (non-zombie) members per process group, from one `/proc` scan.
#[cfg(target_os = "linux")]
pub(super) fn live_groups() -> Option<BTreeMap<i32, usize>> {
    let mut records = Vec::new();
    // A directory-entry error ends `ReadDir`, so the scan is unknown rather
    // than a short count callers would trust as empty. A missing stat is a
    // pid reaped between readdir and the read; that member is gone.
    for entry in std::fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        if let Ok(stat) = std::fs::read(entry.path().join("stat")) {
            records.push(stat);
        }
    }
    parse_stats(
        records.iter().map(Vec::as_slice),
        rustix::process::getpgrp().as_raw_pid(),
    )
}

/// Counts live members per group from `/proc/<pid>/stat` records.
///
/// A record is a `single_open` read: a successful read is the whole line,
/// never a truncated prefix, so a malformed one makes the scan unknown.
/// Zombies cannot run or receive signals; in particular an orphaned zombie
/// waiting for the host's init must not keep a server job alive forever.
/// Without the scanner's own group the scan cannot prove any group empty.
#[cfg(target_os = "linux")]
fn parse_stats<'a>(
    records: impl IntoIterator<Item = &'a [u8]>,
    visible_group: i32,
) -> Option<BTreeMap<i32, usize>> {
    let mut groups = BTreeMap::new();
    let mut visible = false;
    for stat in records {
        // The comm field is raw bytes and may itself contain `)`.
        let split = stat.iter().rposition(|byte| *byte == b')')?;
        let fields = std::str::from_utf8(&stat[split + 1..]).ok()?;
        let mut fields = fields.split_whitespace();
        let state = fields.next()?;
        let _parent = fields.next()?;
        let group = fields.next()?.parse::<i32>().ok()?;
        visible |= group == visible_group;
        if state != "Z" && state != "X" {
            *groups.entry(group).or_default() += 1;
        }
    }
    visible.then_some(groups)
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
fn live_groups() -> Option<BTreeMap<i32, usize>> {
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

#[cfg(target_os = "macos")]
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
#[allow(
    clippy::expect_used,
    reason = "a fixture that cannot be set up fails the test rather than the probe"
)]
mod cleanup_contention_tests {
    use std::cell::Cell;
    use std::os::unix::process::CommandExt;
    // Named here for every platform; the macOS scanner's own import is
    // shadowed rather than repeated.
    use std::process::{Command, Stdio};

    use super::*;

    fn groups(live: &[(i32, usize)]) -> BTreeMap<i32, usize> {
        live.iter().copied().collect()
    }

    /// A clock that advances a millisecond per reading.
    fn ticking(start: Instant) -> impl Fn() -> Instant {
        let ticks = Cell::new(0);
        move || {
            ticks.set(ticks.get() + 1);
            start + Duration::from_millis(ticks.get())
        }
    }

    #[test]
    fn shared_snapshot_counts_all_groups_and_refreshes_at_phase_fences() {
        let mut snapshots = Snapshots::default();
        let first_phase = Instant::now();
        let clock = ticking(first_phase);
        let duplicate = || -> Option<BTreeMap<i32, usize>> { panic!("duplicate scan") };
        assert_eq!(
            snapshots.members(11, first_phase, &clock, false, || Some(groups(&[
                (11, 1),
                (22, 1)
            ]))),
            Some(1)
        );
        assert_eq!(
            snapshots.members(22, first_phase, &clock, false, duplicate),
            Some(1)
        );
        assert_eq!(
            snapshots.members(33, first_phase, &clock, false, duplicate),
            Some(0)
        );
        // A later fence needs a scan begun after it.
        let next_phase = first_phase + Duration::from_millis(100);
        assert_eq!(
            snapshots.members(11, next_phase, || next_phase, false, || Some(groups(&[]))),
            Some(0)
        );
        // Age forces a refresh independently of the caller's older fence.
        let aged = next_phase + Duration::from_millis(250);
        assert_eq!(
            snapshots.members(22, first_phase, || aged, false, || Some(groups(&[(22, 1)]))),
            Some(1)
        );
    }

    #[test]
    fn incomplete_shared_snapshot_never_proves_absence() {
        let mut snapshots = Snapshots::default();
        let phase = Instant::now();
        let clock = ticking(phase);
        assert_eq!(snapshots.members(11, phase, &clock, true, || None), None);
        assert_eq!(
            snapshots.members(22, phase, &clock, true, || panic!("duplicate scan")),
            None
        );
        let later = phase + Duration::from_millis(100);
        assert_eq!(
            snapshots.members(22, later, || later, true, || Some(groups(&[(22, 1)]))),
            Some(1)
        );
    }

    #[test]
    fn an_empty_count_needs_two_consecutive_scans_shared_across_probes() {
        let mut snapshots = Snapshots::default();
        let phase = Instant::now();
        let clock = ticking(phase);
        let scans = Cell::new(0);
        // The first scan misses group 11's member; the confirming scan sees it.
        let missed_then_seen = || {
            scans.set(scans.get() + 1);
            Some(if scans.get() == 1 {
                groups(&[])
            } else {
                groups(&[(11, 1)])
            })
        };
        assert_eq!(
            snapshots.members(11, phase, &clock, true, missed_then_seen),
            Some(1)
        );
        assert_eq!(scans.get(), 2);
        // Both scans began after this probe's fence and both count group 22
        // empty, so it is confirmed without another scan.
        assert_eq!(
            snapshots.members(22, phase, &clock, true, || panic!("duplicate scan")),
            Some(0)
        );
        // A probe fenced after the earlier scan began needs a new confirmation.
        let fence = phase + Duration::from_millis(1) + Duration::from_micros(1);
        assert_eq!(
            snapshots.members(22, fence, &clock, true, || {
                scans.set(scans.get() + 1);
                Some(groups(&[(11, 1)]))
            }),
            Some(0)
        );
        assert_eq!(scans.get(), 3);
        // Without confirmation one empty scan is the answer.
        let mut single = Snapshots::default();
        assert_eq!(
            single.members(11, phase, &clock, false, || Some(groups(&[]))),
            Some(0)
        );
    }

    #[test]
    fn a_shared_probe_confirms_an_empty_count_on_linux_only() {
        let snapshots = Mutex::default();
        let scans = Cell::new(0);
        let missed_then_seen = || {
            scans.set(scans.get() + 1);
            Some(if scans.get() == 1 {
                groups(&[])
            } else {
                groups(&[(11, 1)])
            })
        };
        let counted = shared_members(&snapshots, 11, Instant::now(), missed_then_seen);
        if cfg!(target_os = "linux") {
            assert_eq!((counted, scans.get()), (Some(1), 2));
        } else {
            assert_eq!((counted, scans.get()), (Some(0), 1));
        }
    }

    /// A group that holds only its exited, unreaped leader is empty. macOS
    /// answers killpg(0) on such a group with EPERM, Linux with success.
    #[test]
    fn a_group_holding_only_its_unreaped_leader_reads_empty() {
        let mut leader = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let group = i32::try_from(leader.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .expect("pid");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !super::super::peek_leader_exit(group).is_ok_and(|exit| exit.is_some()) {
            assert!(Instant::now() < deadline, "leader did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(super::super::GroupProbe::new(group).members(), 0);
        assert!(leader.try_wait().expect("reap").is_some());
    }

    /// A leader whose group outlived its cleanup stays unreaped, pinning
    /// the PGID, until the reaper finds the group empty.
    #[test]
    fn a_handed_off_leader_is_reaped_once_its_group_empties() {
        let leader = Command::new("/bin/sh")
            .args(["-c", "sleep 0.5 & exit 0"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let group = i32::try_from(leader.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .expect("pid");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !super::super::peek_leader_exit(group).is_ok_and(|exit| exit.is_some()) {
            assert!(Instant::now() < deadline, "leader did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        super::super::reap_when_group_empty(group, Box::new(leader));
        // The reaper's first pass is a second away and the member still runs.
        assert!(super::super::peek_leader_exit(group).is_ok_and(|exit| exit.is_some()));
        while super::super::peek_leader_exit(group).is_ok() {
            assert!(
                Instant::now() < deadline,
                "the reaper did not reap the leader"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn malformed_ps_output_is_unknown() {
        assert_eq!(
            parse(b"11 S\n11 Z\n22 R+\n", 11),
            Some(groups(&[(11, 1), (22, 1)]))
        );
        for output in [
            b"11 S\n22\n".as_slice(),
            b"invalid R\n",
            b"11 S extra\n",
            b"\xff",
        ] {
            assert!(parse(output, 11).is_none());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn snapshot_without_the_scanners_own_group_cannot_prove_absence() {
        assert!(parse(b"", 11).is_none());
        assert!(parse(b"22 S\n33 Z\n", 11).is_none());
        assert_eq!(parse(b"11 S\n22 Z\n", 11), Some(BTreeMap::from([(11, 1)])));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stats_count_live_members_and_need_the_scanners_own_group() {
        let own = b"10 (test) S 1 11 11 0".as_slice();
        let member = b"20 (a) b) R 10 22 22 0".as_slice();
        let zombie = b"21 (sh) Z 10 22 22 0".as_slice();
        assert_eq!(
            parse_stats([own, member, zombie], 11),
            Some(groups(&[(11, 1), (22, 1)]))
        );
        assert_eq!(parse_stats([own, zombie], 11), Some(groups(&[(11, 1)])));
        // Without the scanner's own group, an empty count proves nothing.
        assert_eq!(parse_stats([zombie], 11), None);
        assert_eq!(parse_stats([], 11), None);
        for malformed in [b"30 (x S".as_slice(), b"30 (x) S 1", b"30 (x) S 1 nan 0"] {
            assert_eq!(parse_stats([own, malformed], 11), None);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_scan_sees_the_scanners_own_group() {
        let own = rustix::process::getpgrp().as_raw_pid();
        let live = live_groups().unwrap_or_default();
        assert!(live.get(&own).is_some_and(|count| *count >= 1));
    }
}
