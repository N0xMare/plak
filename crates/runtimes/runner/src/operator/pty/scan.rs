//! Bounded whole-session scans. A remembered numeric PID never grants kill authority.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(super) fn scan() -> Result<Vec<Identity>, ()> {
    linux::scan()
}

type Snapshot = Option<(Instant, Arc<Vec<Identity>>)>;
const MAX_TRACKED_PROCESSES: usize = 4096;
const STARTUP_SCAN_ATTEMPTS: usize = 3;
// Bounds one Linux whole-table pass. A stat read of another busy process can
// stall, so a loaded host can spend most of a second on one complete pass.
// Each cleanup phase is allotted this bound for every complete pass it needs,
// plus its fixed spacing.
const SCAN_WATCHDOG: Duration = Duration::from_secs(2);
const STAGE_SPACING: Duration = Duration::from_millis(150);
const SCAN_SPACING: Duration = Duration::from_millis(20);
// One pre-signal pass for each of HUP, TERM and KILL, then two before reaping.
const ESCALATION_BUDGET: Duration = SCAN_WATCHDOG
    .saturating_mul(5)
    .saturating_add(STAGE_SPACING.saturating_mul(2));
// Two consecutive complete passes after reaping.
const ABSENCE_PROOF_BUDGET: Duration = SCAN_WATCHDOG.saturating_mul(2).saturating_add(SCAN_SPACING);

pub(super) fn confirm_startup(
    targets: [&Identity; 2],
    mut fresh: impl FnMut() -> Result<Vec<Identity>, ()>,
) -> Result<(), &'static str> {
    // Enumeration failure does not contradict either targeted identity. Each
    // attempt discards every partial row; a completed contradictory scan is
    // not retried and no startup rows are carried into cleanup.
    let rows = (0..STARTUP_SCAN_ATTEMPTS)
        .find_map(|_| fresh().ok())
        .ok_or("operator cleanup scan unavailable")?;
    if targets.iter().all(|target| {
        rows.iter().any(|row| {
            row.pid == target.pid
                && !row.zombie
                && row.started == target.started
                && row.session == target.session
                && row.group == target.group
        })
    }) {
        Ok(())
    } else {
        Err("operator cleanup scan failed isolation probe")
    }
}

/// Running PTYs share observation work. Cleanup deliberately bypasses this
/// cache: both absence proofs and each pre-signal validation are fresh scans.
pub(super) fn observation() -> Result<Arc<Vec<Identity>>, ()> {
    static SNAPSHOT: OnceLock<Mutex<Snapshot>> = OnceLock::new();
    let mut cached = SNAPSHOT
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some((at, rows)) = &*cached
        && at.elapsed() < Duration::from_secs(1)
    {
        return Ok(Arc::clone(rows));
    }
    let rows = Arc::new(scan()?);
    *cached = Some((Instant::now(), Arc::clone(&rows)));
    Ok(rows)
}

#[derive(Clone, Debug)]
pub(super) struct Identity {
    pub(super) pid: u32,
    parent: u32,
    pub(super) group: u32,
    pub(super) session: String,
    pub(super) started: String,
    pub(super) zombie: bool,
    anchor: Option<Arc<AtomicBool>>,
}

impl Identity {
    pub(super) fn capture_child(pid: u32) -> Result<Self, ()> {
        #[cfg(target_os = "linux")]
        let captured = Self::capture(pid);
        #[cfg(target_os = "macos")]
        let captured = Err(());
        Self::from_child_capture(pid, captured)
    }

    pub(super) fn from_child_capture(pid: u32, captured: Result<Self, ()>) -> Result<Self, ()> {
        // std spawn returns success only after portable-pty's setsid/pre_exec
        // and exec completed. The owned, unreaped Child prevents pid/sid/pgid
        // recycling. Darwin need not expose the zombie through ps or waitid.
        #[cfg(target_os = "macos")]
        let mut row = {
            let mut observed = captured.unwrap_or(Self {
                pid,
                parent: std::process::id(),
                group: pid,
                session: pid.to_string(),
                started: String::new(),
                zombie: false,
                anchor: None,
            });
            observed.session = pid.to_string();
            observed.group = pid;
            observed
        };
        #[cfg(target_os = "linux")]
        let mut row = captured?;
        if row.session != pid.to_string() || row.group != pid {
            return Err(());
        }
        row.anchor = Some(Arc::new(AtomicBool::new(true)));
        Ok(row)
    }

    pub(super) fn retire_anchor(&self) {
        if let Some(anchor) = &self.anchor {
            anchor.store(false, Ordering::Release);
        }
    }

    #[cfg(test)]
    pub(super) fn capture_fixture_pid(pid: u32) -> Result<Self, ()> {
        Self::capture(pid)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn capture(pid: u32) -> Result<Self, ()> {
        linux::capture(pid)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn capture(pid: u32) -> Result<Self, ()> {
        let mut rows = scan_ps(Some(pid))?;
        let row = rows.pop().ok_or(())?;
        (rows.is_empty() && row.pid == pid).then_some(row).ok_or(())
    }
}

pub(super) struct SessionCleanup {
    leader: Identity,
    seen: HashMap<u32, String>,
    pub(super) escaped: HashMap<u32, String>,
    session_live: HashMap<u32, String>,
    pub(super) uncertain: bool,
}

enum Escalation {
    Signal(Vec<Identity>),
    Reap,
}

impl SessionCleanup {
    pub(super) fn new(leader: Identity) -> Self {
        Self {
            seen: HashMap::from([(leader.pid, leader.started.clone())]),
            escaped: HashMap::new(),
            session_live: HashMap::new(),
            leader,
            uncertain: false,
        }
    }

    pub(super) fn observe(&mut self, rows: &[Identity]) {
        self.session_live = rows
            .iter()
            .filter(|row| row.session == self.leader.session && !row.zombie)
            .take(MAX_TRACKED_PROCESSES)
            .map(|row| (row.pid, row.started.clone()))
            .collect();
        if rows
            .iter()
            .filter(|row| row.session == self.leader.session && !row.zombie)
            .count()
            > MAX_TRACKED_PROCESSES
        {
            self.uncertain = true;
        }
        if self.leader.started.is_empty()
            && self
                .leader
                .anchor
                .as_ref()
                .is_some_and(|anchor| anchor.load(Ordering::Acquire))
            && let Some(row) = rows.iter().find(|row| row.pid == self.leader.pid)
        {
            self.leader.started.clone_from(&row.started);
            self.seen.insert(row.pid, row.started.clone());
        }
        let current: HashSet<_> = rows
            .iter()
            .map(|row| (row.pid, row.started.as_str()))
            .collect();
        self.escaped
            .retain(|pid, start| current.contains(&(*pid, start.as_str())));
        self.seen
            .retain(|pid, start| current.contains(&(*pid, start.as_str())));
        for _ in 0..rows.len().min(32) {
            let prior = self.seen.len();
            for row in rows {
                if row.session == self.leader.session || self.seen.contains_key(&row.parent) {
                    if self.seen.len() >= MAX_TRACKED_PROCESSES && !self.seen.contains_key(&row.pid)
                    {
                        self.uncertain = true;
                        continue;
                    }
                    self.seen
                        .entry(row.pid)
                        .or_insert_with(|| row.started.clone());
                }
                if self.seen.get(&row.pid) == Some(&row.started)
                    && row.session != self.leader.session
                {
                    self.escaped.insert(row.pid, row.started.clone());
                }
            }
            if self.seen.len() == prior {
                break;
            }
        }
    }

    pub(super) fn finish(&mut self, mut reap: impl FnMut() -> bool) -> bool {
        let observed_escape = !self.escaped.is_empty();
        let Some(certain) = self.signal_live_groups_until(Instant::now() + ESCALATION_BUDGET)
        else {
            return false;
        };
        self.leader.retire_anchor();
        if !reap() {
            return false;
        }
        // Absence scans cannot turn an observed escape or an unconfirmed
        // signal into proof, so they do not delay the unknown result.
        if !certain || observed_escape || !self.escaped.is_empty() {
            return false;
        }
        self.prove_absence_until(Instant::now() + ABSENCE_PROOF_BUDGET)
    }

    fn signal_live_groups_until(&mut self, deadline: Instant) -> Option<bool> {
        let mut certain = true;
        let mut fresh = || complete_scan(deadline);
        for signal in [
            rustix::process::Signal::HUP,
            rustix::process::Signal::TERM,
            rustix::process::Signal::KILL,
        ] {
            let Escalation::Signal(rows) = self.escalation_step(&mut fresh, child_exit_status)?
            else {
                return Some(certain);
            };
            certain &= self.signal_groups(&rows, signal);
            if signal != rustix::process::Signal::KILL {
                std::thread::sleep(STAGE_SPACING);
            }
        }
        // Repeat KILL from a fresh deduplicated scan for members forked during
        // escalation. Retain the unreaped child as SID anchor through every signal.
        loop {
            let Escalation::Signal(rows) = self.escalation_step(&mut fresh, child_exit_status)?
            else {
                return Some(certain);
            };
            certain &= self.signal_groups(&rows, rustix::process::Signal::KILL);
            std::thread::sleep(SCAN_SPACING);
        }
    }

    fn escalation_step(
        &mut self,
        mut fresh: impl FnMut() -> Option<Vec<Identity>>,
        exit_status: impl Fn(u32) -> Result<bool, rustix::io::Errno>,
    ) -> Option<Escalation> {
        // A fork relay can evade one enumeration. Only two fresh empty scans
        // of an exited, still anchored leader permit reaping without another
        // signal. Neither scan counts toward the post-reap absence proof.
        for _ in 0..2 {
            let rows = fresh()?;
            if !self.leader_anchored(&rows) {
                return None;
            }
            // On Linux a matching scan row alone cannot prove ownership if
            // waitid reports ECHILD. Do not signal or reap on that evidence.
            let exited = exit_status(self.leader.pid).ok()?;
            if !exited || self.has_live_members(&rows) {
                return Some(Escalation::Signal(rows));
            }
            self.observe(&rows);
        }
        Some(Escalation::Reap)
    }

    fn has_live_members(&self, rows: &[Identity]) -> bool {
        rows.iter()
            .any(|row| row.session == self.leader.session && !row.zombie)
    }

    fn signal_groups(&mut self, rows: &[Identity], signal: rustix::process::Signal) -> bool {
        self.signal_groups_with(rows, signal, rustix::process::kill_process_group)
    }

    fn signal_groups_with(
        &mut self,
        rows: &[Identity],
        signal: rustix::process::Signal,
        mut send: impl FnMut(
            rustix::process::Pid,
            rustix::process::Signal,
        ) -> Result<(), rustix::io::Errno>,
    ) -> bool {
        self.observe(rows);
        // A PTY child is its own session leader. Never accept the runner's SID,
        // a ps sess pointer, or a reused leader as process-group kill authority.
        if !self.leader_anchored(rows) {
            return false;
        }
        let groups: HashSet<_> = rows
            .iter()
            .filter(|row| row.session == self.leader.session && !row.zombie)
            .map(|row| row.group)
            .collect();
        let mut certain = true;
        for group in groups {
            // An uncertain group cannot prevent cleanup of other validated groups.
            certain &= self.signal_group(rows, group, signal, &mut send).is_ok();
        }
        certain
    }

    fn signal_group(
        &self,
        rows: &[Identity],
        group: u32,
        signal: rustix::process::Signal,
        send: &mut impl FnMut(
            rustix::process::Pid,
            rustix::process::Signal,
        ) -> Result<(), rustix::io::Errno>,
    ) -> Result<(), ()> {
        // Validate every member against this same fresh scan before signalling.
        if rows.iter().any(|row| {
            row.group == group
                && row.session != self.leader.session
                && (!cfg!(target_os = "macos") || !row.zombie)
        }) {
            return Err(());
        }
        let only_anchor = self.leader_anchored(rows)
            && rows
                .iter()
                .filter(|row| row.group == group)
                .all(|row| row.pid == self.leader.pid);
        let mut validated = false;
        for member in rows.iter().filter(|row| row.group == group && !row.zombie) {
            let Some(pid) = i32::try_from(member.pid)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
            else {
                return Err(());
            };
            match rustix::process::getsid(Some(pid)) {
                Ok(sid) if sid.as_raw_nonzero().get().to_string() == self.leader.session => {}
                Err(rustix::io::Errno::SRCH) => continue,
                Err(rustix::io::Errno::PERM) if only_anchor => continue,
                _ => return Err(()),
            }
            let Some(expected) = i32::try_from(group)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
            else {
                return Err(());
            };
            match rustix::process::getpgid(Some(pid)) {
                Ok(actual) if actual == expected => {}
                Err(rustix::io::Errno::SRCH) => continue,
                Err(rustix::io::Errno::PERM) if only_anchor => continue,
                _ => return Err(()),
            }
            validated = true;
        }
        if !validated {
            return Ok(());
        }
        let Some(group) = i32::try_from(group)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        else {
            return Err(());
        };
        if !self.leader_anchored(rows) {
            return Err(());
        }
        if let Err(error) = send(group, signal)
            && error != rustix::io::Errno::SRCH
            && !(error == rustix::io::Errno::PERM && only_anchor)
        {
            return Err(());
        }
        Ok(())
    }

    pub(super) fn leader_anchored(&self, rows: &[Identity]) -> bool {
        if self.leader.session != self.leader.pid.to_string() {
            return false;
        }
        if self
            .leader
            .anchor
            .as_ref()
            .is_some_and(|anchor| !anchor.load(Ordering::Acquire))
        {
            return false;
        }
        if let Some(row) = rows.iter().find(|row| row.pid == self.leader.pid) {
            if !self.leader.started.is_empty() && row.started != self.leader.started {
                return false;
            }
            if cfg!(target_os = "linux") && row.session == self.leader.session {
                return true;
            }
        }
        // Darwin uses the owned handle, never waitid's unreliable si_pid. Linux
        // still requires the full scanned SID/start identity above.
        cfg!(target_os = "macos")
            && self
                .leader
                .anchor
                .as_ref()
                .is_some_and(|anchor| anchor.load(Ordering::Acquire))
    }

    pub(super) fn reprove(&mut self) -> bool {
        self.prove_absence_until(Instant::now() + Duration::from_secs(1))
    }

    pub(super) fn survivors(&self) -> Vec<super::super::TerminalProcessIdentity> {
        let mut identities = self.session_live.clone();
        identities.extend(
            self.escaped
                .iter()
                .map(|(pid, start)| (*pid, start.clone())),
        );
        let mut survivors: Vec<_> = identities
            .iter()
            .map(|(&pid, started)| super::super::TerminalProcessIdentity {
                pid,
                started: started.clone(),
            })
            .collect();
        survivors.sort_by_key(|identity| identity.pid);
        survivors.truncate(MAX_TRACKED_PROCESSES);
        survivors
    }

    pub(super) fn reprove_once(&mut self) -> bool {
        self.prove_pair(scan)
    }

    fn prove_pair(&mut self, mut scan: impl FnMut() -> Result<Vec<Identity>, ()>) -> bool {
        let mut absent = true;
        for step in 0..2 {
            if step != 0 {
                std::thread::sleep(Duration::from_millis(20));
            }
            absent &= scan().is_ok_and(|rows| self.absent_in(&rows));
        }
        absent && !self.uncertain
    }

    fn absent_in(&mut self, rows: &[Identity]) -> bool {
        self.observe(rows);
        self.escaped.retain(|pid, start| {
            rows.iter()
                .any(|row| row.pid == *pid && row.started == *start && !row.zombie)
        });
        !rows.iter().any(|r| {
            r.session == self.leader.session || self.escaped.get(&r.pid) == Some(&r.started)
        })
    }

    fn prove_absence_until(&mut self, deadline: Instant) -> bool {
        let mut absent = 0;
        while Instant::now() < deadline {
            let Ok(rows) = scan() else {
                absent = 0;
                std::thread::sleep(SCAN_SPACING);
                continue;
            };
            if self.absent_in(&rows) {
                absent += 1;
            } else {
                absent = 0;
            }
            if absent == 2 {
                return !self.uncertain;
            }
            std::thread::sleep(SCAN_SPACING);
        }
        false
    }
}

/// Retries incomplete passes within the tests' liveness bound.
#[cfg(test)]
pub(super) fn complete_scan_for_test() -> Vec<Identity> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(rows) = scan() {
            return rows;
        }
        assert!(
            Instant::now() < deadline,
            "no complete process-table scan within the liveness bound"
        );
        std::thread::sleep(SCAN_SPACING);
    }
}

fn complete_scan(deadline: Instant) -> Option<Vec<Identity>> {
    while Instant::now() < deadline {
        if let Ok(rows) = scan() {
            return Some(rows);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

pub(super) fn child_exited(pid: u32) -> bool {
    child_exit_status(pid).is_ok_and(|exited| exited)
}

fn child_exit_status(pid: u32) -> Result<bool, rustix::io::Errno> {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or(rustix::io::Errno::INVAL)?;
    #[cfg(target_os = "macos")]
    {
        match rustix::process::getsid(Some(pid)) {
            Ok(_) => Ok(false),
            Err(rustix::io::Errno::SRCH) => Ok(true),
            Err(error) => Err(error),
        }
    }
    #[cfg(target_os = "linux")]
    {
        rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        )
        .map(|status| status.is_some())
    }
}

#[cfg(target_os = "macos")]
pub(super) fn scan() -> Result<Vec<Identity>, ()> {
    scan_ps(None)
}

#[cfg(target_os = "macos")]
fn scan_ps(target: Option<u32>) -> Result<Vec<Identity>, ()> {
    use std::io::Read as _;
    let mut command = std::process::Command::new("/bin/ps");
    if let Some(pid) = target {
        command.arg("-p").arg(pid.to_string());
    } else {
        command.arg("-ax");
    }
    let mut child = command
        .args(["-o", "pid=,ppid=,pgid=,state=,lstart="])
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    let result = (|| {
        let mut stdout = child.stdout.take().ok_or(())?;
        rustix::fs::fcntl_setfl(&stdout, rustix::fs::OFlags::NONBLOCK).map_err(|_| ())?;
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut bytes = Vec::new();
        let mut buffer = [0; 16384];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return Err(()),
            }
            if bytes.len() > 8 * 1024 * 1024 || Instant::now() > deadline {
                return Err(());
            }
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| ())?;
        let mut rows = Vec::new();
        for line in text.lines() {
            let f: Vec<_> = line.split_whitespace().collect();
            if f.len() != 9 {
                return Err(());
            }
            let pid: u32 = f[0].parse().map_err(|_| ())?;
            let pid_arg = i32::try_from(pid)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
                .ok_or(())?;
            // Darwin's ps `sess` is not the POSIX session id (it may be 0).
            let session = match rustix::process::getsid(Some(pid_arg)) {
                Ok(session) => session.as_raw_nonzero().get().to_string(),
                Err(rustix::io::Errno::SRCH) if f[3].starts_with('Z') => String::new(),
                Err(rustix::io::Errno::SRCH) => continue,
                Err(_) => return Err(()),
            };
            let group = f[2].parse().map_err(|_| ())?;
            let zombie = f[3].starts_with('Z');
            if target.is_some()
                && !zombie
                && !rustix::process::getpgid(Some(pid_arg))
                    .is_ok_and(|pgid| pgid.as_raw_nonzero().get().cast_unsigned() == group)
            {
                return Err(());
            }
            rows.push(Identity {
                pid,
                parent: f[1].parse().map_err(|_| ())?,
                group,
                session,
                zombie,
                // lstart has only one-second resolution on macOS. An unreaped
                // direct-child leader, not this timestamp alone, anchors signals.
                started: f[4..].join(" "),
                anchor: None,
            });
        }
        Ok(rows)
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|_| ())?;
    if !status.success() {
        return Err(());
    }
    result
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn scan() -> Result<Vec<Identity>, ()> {
    Err(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "identity and escape fixtures")]
mod tests {
    use super::*;

    fn row(pid: u32, parent: u32, session: &str) -> Identity {
        Identity {
            pid,
            parent,
            group: pid,
            session: session.into(),
            started: format!("start-{pid}"),
            zombie: false,
            anchor: None,
        }
    }

    #[test]
    fn operator_startup_retries_incomplete_enumeration_but_not_identity_disagreement() {
        let runner = row(5, 1, "1");
        let child = row(10, 5, "10");
        let targets = [&runner, &child];
        let mut attempts = 0;
        assert_eq!(
            confirm_startup(targets, || {
                attempts += 1;
                if attempts < 3 {
                    Err(())
                } else {
                    Ok(vec![runner.clone(), child.clone()])
                }
            }),
            Ok(())
        );
        assert_eq!(attempts, 3);

        attempts = 0;
        assert_eq!(
            confirm_startup(targets, || {
                attempts += 1;
                Err(())
            }),
            Err("operator cleanup scan unavailable")
        );
        assert_eq!(attempts, 3);

        for contradict in [
            row(11, 5, "10"),
            row(10, 5, "5"),
            Identity {
                started: "reused".into(),
                ..child.clone()
            },
            Identity {
                group: 11,
                ..child.clone()
            },
            Identity {
                zombie: true,
                ..child.clone()
            },
        ] {
            attempts = 0;
            assert_eq!(
                confirm_startup(targets, || {
                    attempts += 1;
                    Ok(vec![runner.clone(), contradict.clone()])
                }),
                Err("operator cleanup scan failed isolation probe")
            );
            assert_eq!(attempts, 1);
        }
    }

    fn zombie_anchor() -> Identity {
        let mut leader = row(10, 1, "10");
        leader.zombie = true;
        leader.anchor = Some(Arc::new(AtomicBool::new(true)));
        leader
    }

    #[test]
    fn operator_cleanup_signals_members_found_in_either_fresh_scan() {
        for first_empty in [false, true] {
            let leader = zombie_anchor();
            let live = vec![leader.clone(), row(11, 10, "10")];
            let mut scans = [first_empty.then(|| vec![leader.clone()]), Some(live)]
                .into_iter()
                .flatten();
            let mut cleanup = SessionCleanup::new(leader);
            assert!(matches!(
                cleanup.escalation_step(|| scans.next(), |_| Ok(true)),
                Some(Escalation::Signal(rows)) if rows.iter().any(|row| row.pid == 11)
            ));
            assert!(scans.next().is_none());
        }
    }

    #[test]
    fn operator_cleanup_only_skips_signals_after_two_empty_scans_of_exited_leader() {
        let leader = zombie_anchor();
        let mut cleanup = SessionCleanup::new(leader.clone());
        assert!(matches!(
            cleanup.escalation_step(|| Some(vec![leader.clone()]), |_| Ok(false)),
            Some(Escalation::Signal(_))
        ));
        assert!(
            cleanup
                .escalation_step(
                    || Some(vec![leader.clone()]),
                    |_| Err(rustix::io::Errno::CHILD)
                )
                .is_none(),
            "ECHILD must refuse anchoring even with a matching stale scan"
        );
        let mut one_scan = Some(vec![leader.clone()]);
        assert!(
            cleanup
                .escalation_step(|| one_scan.take(), |_| Ok(true))
                .is_none()
        );
        let mut scans = [vec![leader.clone()], vec![leader]].into_iter();
        assert!(matches!(
            cleanup.escalation_step(|| scans.next(), |_| Ok(true)),
            Some(Escalation::Reap)
        ));
        assert!(scans.next().is_none());
    }

    #[test]
    fn operator_cleanup_detects_observed_setsid_escape_and_keeps_uncertainty() {
        let mut cleanup = SessionCleanup::new(row(10, 1, "10"));
        cleanup.observe(&[row(10, 1, "10"), row(11, 10, "10")]);
        cleanup.observe(&[row(10, 1, "10"), row(11, 10, "11")]);
        assert!(!cleanup.escaped.is_empty());
    }

    #[test]
    fn operator_pty_signal_groups_rejects_forged_members_and_continues_valid_groups() {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new("/bin/sleep");
        command.arg("5");
        let mut child = pair.slave.spawn_command(command).unwrap();
        let leader = Identity::capture_child(child.process_id().unwrap()).unwrap();
        let snapshot = complete_scan_for_test();
        let live_leader = snapshot
            .iter()
            .find(|r| r.pid == leader.pid)
            .unwrap()
            .clone();
        let own = snapshot
            .iter()
            .find(|r| r.pid == std::process::id())
            .unwrap()
            .clone();
        let mut cleanup = SessionCleanup::new(leader);
        // A forged SID cannot turn this test process's real group into a target.
        let mut forged = own.clone();
        forged.session.clone_from(&live_leader.session);
        let mut zombie_leader = live_leader.clone();
        zombie_leader.zombie = true;
        let mut sent = Vec::new();
        assert!(!cleanup.signal_groups_with(
            &[zombie_leader, forged.clone()],
            rustix::process::Signal::HUP,
            |group, _| {
                sent.push(group);
                Ok(())
            }
        ));
        assert_eq!(sent, Vec::<rustix::process::Pid>::new());
        // A foreign-session row in the candidate group refuses that entire group.
        let mut foreign = own;
        foreign.group = live_leader.group;
        assert!(!cleanup.signal_groups_with(
            &[live_leader.clone(), foreign],
            rustix::process::Signal::HUP,
            |group, _| {
                sent.push(group);
                Ok(())
            }
        ));
        assert_eq!(sent, Vec::<rustix::process::Pid>::new());
        // An uncertain independent group does not suppress a valid leader group.
        assert!(!cleanup.signal_groups_with(
            &[live_leader.clone(), forged],
            rustix::process::Signal::HUP,
            |group, _| {
                sent.push(group);
                Ok(())
            }
        ));
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].as_raw_nonzero().get().unsigned_abs(),
            live_leader.group
        );
        // Darwin can return EPERM when the only scanned member is our anchor.
        assert!(cleanup.signal_groups_with(
            &[live_leader],
            rustix::process::Signal::HUP,
            |_, _| Err(rustix::io::Errno::PERM)
        ));
        drop(pair);
        let mut cleanup = SessionCleanup::new(cleanup.leader);
        assert!(cleanup.finish(|| child.try_wait().ok().flatten().is_some()));
        child.wait().unwrap();
    }

    #[test]
    fn operator_cleanup_unknown_survivors_include_last_live_session_members() {
        let mut cleanup = SessionCleanup::new(row(10, 1, "10"));
        let mut zombie = row(10, 1, "10");
        zombie.zombie = true;
        cleanup.observe(&[zombie, row(20, 10, "10"), row(30, 10, "30")]);
        let survivors = cleanup.survivors();
        assert_eq!(
            survivors.iter().map(|r| r.pid).collect::<Vec<_>>(),
            [20, 30]
        );
        assert!(
            survivors
                .iter()
                .all(|r| r.started == format!("start-{}", r.pid))
        );
    }

    #[test]
    fn operator_cleanup_periodic_proof_uses_only_one_scan_pair() {
        let mut cleanup = SessionCleanup::new(row(10, 1, "10"));
        let mut calls = 0;
        assert!(!cleanup.prove_pair(|| {
            calls += 1;
            Err(())
        }));
        assert_eq!(
            calls, 2,
            "periodic incomplete scans do not retry in a window"
        );
        calls = 0;
        assert!(cleanup.prove_pair(|| {
            calls += 1;
            Ok(Vec::new())
        }));
        assert_eq!(calls, 2);
    }

    #[test]
    fn operator_runner_loss_pid_reuse_never_authorizes_a_signal() {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new("/bin/sleep");
        command.arg("1");
        let mut child = pair.slave.spawn_command(command).unwrap();
        let mut current = Identity::capture_child(child.process_id().unwrap()).unwrap();
        assert_eq!(
            current.session,
            current.pid.to_string(),
            "a real PTY session leader exercises the start-identity fence"
        );
        current.started = "wrong-start-identity".into();
        let mut cleanup = SessionCleanup::new(current);
        assert!(!cleanup.finish(|| panic!("stale identity must not even reap")));
        cleanup.leader.retire_anchor();
        drop(pair);
        child.wait().unwrap();
    }

    #[test]
    fn operator_cleanup_tracking_drops_old_identities_and_bounds_live_members() {
        let leader = row(10, 1, "10");
        let mut cleanup = SessionCleanup::new(leader.clone());
        for pid in 100..5000 {
            cleanup.observe(&[leader.clone(), row(pid, 10, "10")]);
            assert_eq!(
                cleanup.seen.len(),
                2,
                "exited child identities must not accumulate"
            );
        }
        let mut crowded = vec![leader];
        crowded.extend((100..4300).map(|pid| row(pid, 10, "10")));
        cleanup.observe(&crowded);
        assert_eq!(cleanup.seen.len(), MAX_TRACKED_PROCESSES);
        assert!(
            cleanup.uncertain,
            "a tracking limit is not proof that untracked descendants died"
        );
    }
}
