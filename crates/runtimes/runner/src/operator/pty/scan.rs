//! Bounded whole-session scans. A remembered numeric PID never grants kill authority.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

type Snapshot = Option<(Instant, Arc<Vec<Identity>>)>;
const MAX_TRACKED_PROCESSES: usize = 4096;

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
    pub(super) fn capture(pid: u32) -> Result<Self, ()> {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if let Ok(rows) = scan()
                && let Some(row) = rows.into_iter().find(|row| row.pid == pid)
            {
                return Ok(row);
            }
            if Instant::now() >= deadline {
                return Err(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub(super) struct SessionCleanup {
    leader: Identity,
    seen: HashMap<u32, String>,
    pub(super) escaped: HashMap<u32, String>,
    session_live: HashMap<u32, String>,
    pub(super) uncertain: bool,
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
        let mut certain = true;
        let deadline = Instant::now() + Duration::from_secs(3);
        for signal in [
            rustix::process::Signal::HUP,
            rustix::process::Signal::TERM,
            rustix::process::Signal::KILL,
        ] {
            let Some(rows) = complete_scan(deadline) else {
                return false;
            };
            if !self.leader_anchored(&rows) {
                return false;
            }
            certain &= self.signal_groups(&rows, signal);
            if signal != rustix::process::Signal::KILL {
                std::thread::sleep(Duration::from_millis(150));
            }
        }
        // Repeat KILL from a fresh deduplicated scan for members forked during
        // escalation. Retain the unreaped child as SID anchor through every signal.
        loop {
            let Some(rows) = complete_scan(deadline) else {
                return false;
            };
            if !rows
                .iter()
                .any(|r| r.session == self.leader.session && !r.zombie)
            {
                break;
            }
            if !self.leader_anchored(&rows) {
                return false;
            }
            certain &= self.signal_groups(&rows, rustix::process::Signal::KILL);
            std::thread::sleep(Duration::from_millis(20));
        }
        self.leader.retire_anchor();
        if !reap() {
            return false;
        }
        self.prove_absence_until(deadline) && certain && !observed_escape && self.escaped.is_empty()
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
                std::thread::sleep(Duration::from_millis(20));
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
            std::thread::sleep(Duration::from_millis(20));
        }
        false
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
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    #[cfg(target_os = "macos")]
    {
        matches!(
            rustix::process::getsid(Some(pid)),
            Err(rustix::io::Errno::SRCH)
        )
    }
    #[cfg(target_os = "linux")]
    {
        rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        )
        .is_ok_and(|status| status.is_some())
    }
}

#[cfg(target_os = "linux")]
pub(super) fn scan() -> Result<Vec<Identity>, ()> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut rows = Vec::new();
    for (count, entry) in std::fs::read_dir("/proc").map_err(|_| ())?.enumerate() {
        if count > 65536 || Instant::now() > deadline {
            return Err(());
        }
        let entry = entry.map_err(|_| ())?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let bytes = match std::fs::read(entry.path().join("stat")) {
            Ok(text) if text.len() <= 65536 => text,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
            {
                continue;
            }
            _ => return Err(()),
        };
        // A task reaped between open and read can yield empty/truncated stat.
        // It has vanished from this observation, not failed the entire scan.
        if let Ok(identity) = parse_stat(pid, &String::from_utf8_lossy(&bytes)) {
            rows.push(identity);
        }
    }
    Ok(rows)
}

#[cfg(target_os = "linux")]
fn parse_stat(pid: u32, text: &str) -> Result<Identity, ()> {
    let (_, fields) = text.rsplit_once(") ").ok_or(())?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    if fields.len() < 20 {
        return Err(());
    }
    Ok(Identity {
        pid,
        parent: fields[1].parse().map_err(|_| ())?,
        group: fields[2].parse().map_err(|_| ())?,
        session: fields[3].into(),
        started: fields[19].into(),
        zombie: fields[0] == "Z",
        anchor: None,
    })
}

#[cfg(target_os = "macos")]
pub(super) fn scan() -> Result<Vec<Identity>, ()> {
    use std::io::Read as _;
    let mut child = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,pgid=,state=,lstart="])
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
            rows.push(Identity {
                pid,
                parent: f[1].parse().map_err(|_| ())?,
                group: f[2].parse().map_err(|_| ())?,
                session,
                zombie: f[3].starts_with('Z'),
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
        let snapshot = scan().unwrap();
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
