//! Bounded whole-session scans. A remembered numeric PID never grants kill authority.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

type Snapshot = Option<(Instant, Arc<Vec<Identity>>)>;

/// Running PTYs share observation work. Cleanup deliberately bypasses this
/// cache: both absence proofs and each pre-signal validation are fresh scans.
pub(super) fn observation() -> Result<Arc<Vec<Identity>>, ()> {
    static SNAPSHOT: OnceLock<Mutex<Snapshot>> = OnceLock::new();
    let mut cached = SNAPSHOT
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some((at, rows)) = &*cached
        && at.elapsed() < Duration::from_millis(50)
    {
        return Ok(Arc::clone(rows));
    }
    let rows = Arc::new(scan()?);
    *cached = Some((Instant::now(), Arc::clone(&rows)));
    Ok(rows)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Identity {
    pub(super) pid: u32,
    parent: u32,
    pub(super) group: u32,
    pub(super) session: String,
    started: String,
    pub(super) zombie: bool,
}

impl Identity {
    pub(super) fn capture(pid: u32) -> Result<Self, ()> {
        scan()?.into_iter().find(|row| row.pid == pid).ok_or(())
    }
}

pub(super) struct SessionCleanup {
    leader: Identity,
    seen: HashMap<u32, String>,
    pub(super) uncertain: bool,
}

impl SessionCleanup {
    pub(super) fn new(leader: Identity) -> Self {
        Self {
            seen: HashMap::from([(leader.pid, leader.started.clone())]),
            leader,
            uncertain: false,
        }
    }

    pub(super) fn observe(&mut self, rows: &[Identity]) {
        for _ in 0..rows.len().min(32) {
            let prior = self.seen.len();
            for row in rows {
                if row.session == self.leader.session || self.seen.contains_key(&row.parent) {
                    self.seen
                        .entry(row.pid)
                        .or_insert_with(|| row.started.clone());
                }
                if self.seen.get(&row.pid) == Some(&row.started)
                    && row.session != self.leader.session
                {
                    self.uncertain = true;
                }
            }
            if self.seen.len() == prior {
                break;
            }
        }
    }

    pub(super) fn finish(&mut self, mut reap: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        for signal in [
            rustix::process::Signal::HUP,
            rustix::process::Signal::TERM,
            rustix::process::Signal::KILL,
        ] {
            let Ok(rows) = scan() else {
                return false;
            };
            self.observe(&rows);
            // The unreaped leader anchors this SID, including after shell exit.
            if !rows
                .iter()
                .any(|r| r.pid == self.leader.pid && r.started == self.leader.started)
            {
                return false;
            }
            for row in rows
                .iter()
                .filter(|r| r.session == self.leader.session && !r.zombie)
            {
                if Instant::now() >= deadline {
                    return false;
                }
                let Ok(fresh) = scan() else {
                    return false;
                };
                if !fresh.iter().any(|r| {
                    r.pid == row.pid
                        && r.started == row.started
                        && r.group == row.group
                        && r.session == self.leader.session
                }) {
                    continue;
                }
                let Some(group) = i32::try_from(row.group)
                    .ok()
                    .and_then(rustix::process::Pid::from_raw)
                else {
                    return false;
                };
                if let Err(error) = rustix::process::kill_process_group(group, signal)
                    && error != rustix::io::Errno::SRCH
                {
                    self.uncertain = true;
                }
            }
            let grace = Instant::now() + Duration::from_millis(150);
            while Instant::now() < grace {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        // Release the leader only after the last signal, then prove literal SID
        // absence twice. No signal is ever sent after releasing that PID anchor.
        if !reap() {
            return false;
        }
        let mut absent = 0;
        while Instant::now() < deadline {
            let Ok(rows) = scan() else {
                return false;
            };
            self.observe(&rows);
            if rows.iter().any(|r| r.session == self.leader.session) {
                absent = 0;
            } else {
                absent += 1;
            }
            if absent == 2 {
                return !self.uncertain;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
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
        let text = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(text) if text.len() <= 65536 => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            _ => return Err(()),
        };
        rows.push(parse_stat(pid, &text)?);
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
    })
}

#[cfg(target_os = "macos")]
pub(super) fn scan() -> Result<Vec<Identity>, ()> {
    use std::io::Read as _;
    let mut child = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,pgid=,sess=,state=,lstart="])
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
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(_) => return Err(()),
            }
            if bytes.len() > 8 * 1024 * 1024 || Instant::now() > deadline {
                return Err(());
            }
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| ())?;
        text.lines()
            .map(|line| {
                let f: Vec<_> = line.split_whitespace().collect();
                if f.len() != 10 {
                    return Err(());
                }
                Ok(Identity {
                    pid: f[0].parse().map_err(|_| ())?,
                    parent: f[1].parse().map_err(|_| ())?,
                    group: f[2].parse().map_err(|_| ())?,
                    session: f[3].into(),
                    zombie: f[4].starts_with('Z'),
                    started: f[5..].join(" "),
                })
            })
            .collect()
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
        }
    }

    #[test]
    fn operator_cleanup_detects_observed_setsid_escape_and_keeps_uncertainty() {
        let mut cleanup = SessionCleanup::new(row(10, 1, "10"));
        cleanup.observe(&[row(10, 1, "10"), row(11, 10, "10")]);
        cleanup.observe(&[row(10, 1, "10"), row(11, 10, "11")]);
        assert!(cleanup.uncertain);
    }

    #[test]
    fn operator_runner_loss_pid_reuse_never_authorizes_a_signal() {
        let mut current = Identity::capture(std::process::id()).unwrap();
        current.started = "wrong-start-identity".into();
        assert!(
            !SessionCleanup::new(current).finish(|| panic!("stale identity must not even reap"))
        );
    }
}
