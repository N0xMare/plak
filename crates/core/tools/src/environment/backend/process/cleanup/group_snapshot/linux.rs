//! Linux shares kernel membership observations, reading state only on demand.

use super::{GroupCounts, complete_stat, live_groups, stat_group};
use nix::errno::Errno as PidError;
use nix::unistd::{Pid, getpgid, getpgrp, getpid};
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{Dir, Mode, OFlags, PROC_SUPER_MAGIC, fstatfs, open, openat};
use rustix::io::{Errno, read};
use std::collections::BTreeMap;

const READ: OFlags = OFlags::RDONLY.union(OFlags::CLOEXEC);

pub(super) struct ProcSnapshot(Observation);

enum Observation {
    Kernel {
        proc: OwnedFd,
        pids: BTreeMap<i32, Vec<Pid>>,
        counts: BTreeMap<i32, Option<usize>>,
    },
    Full(BTreeMap<i32, usize>),
}

impl GroupCounts for ProcSnapshot {
    fn count(&mut self, group: i32) -> Option<usize> {
        match &mut self.0 {
            Observation::Kernel { proc, pids, counts } => *counts
                .entry(group)
                .or_insert_with(|| count_live(proc, pids.get(&group).map(Vec::as_slice), group)),
            Observation::Full(counts) => counts.count(group),
        }
    }

    fn counts_at_scan(&self) -> bool {
        matches!(self.0, Observation::Full(_))
    }
}

pub(super) fn snapshot() -> Option<ProcSnapshot> {
    let proc = open("/proc", READ | OFlags::DIRECTORY, Mode::empty()).ok()?;
    let own_pid = getpid();
    let own_group = getpgrp();
    if !fstatfs(&proc).is_ok_and(|stat| stat.f_type == PROC_SUPER_MAGIC)
        || !same_pid_namespace(&proc, own_pid)
    {
        return live_groups().map(|groups| ProcSnapshot(Observation::Full(groups)));
    }
    let mut pids = BTreeMap::<i32, Vec<Pid>>::new();
    let mut visible = false;
    for entry in Dir::read_from(&proc).ok()? {
        let entry = entry.ok()?;
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };
        if !name.as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let Ok(raw_pid) = name.parse::<i32>() else {
            continue;
        };
        if raw_pid == 0 {
            continue;
        }
        let pid = Pid::from_raw(raw_pid);
        // Linux returns PGID zero for kernel tasks. Use the wrapper that
        // represents zero rather than a nonzero process-ID type.
        let group = match getpgid(Some(pid)) {
            Ok(group) if group.as_raw() == 0 => continue,
            Ok(group) => group,
            Err(PidError::ESRCH) => continue,
            Err(_) => return None,
        };
        visible |= pid == own_pid && group == own_group;
        pids.entry(group.as_raw()).or_default().push(pid);
    }
    visible.then_some(ProcSnapshot(Observation::Kernel {
        proc,
        pids,
        counts: BTreeMap::new(),
    }))
}

fn same_pid_namespace(proc: impl AsFd, own_pid: Pid) -> bool {
    let mut buffer = [0; 4096];
    let status =
        openat(proc, "self/status", READ, Mode::empty()).and_then(|fd| read(fd, &mut buffer[..]));
    status.is_ok_and(|length| pid_in_own_namespace(&buffer[..length], own_pid))
}

fn pid_in_own_namespace(status: &[u8], own_pid: Pid) -> bool {
    // NSpid starts with the procfs mount's PID namespace and lists each
    // nested namespace. Matching a number alone cannot rule out an ancestor.
    status
        .split_inclusive(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_suffix(b"\n"))
        .find_map(|line| line.strip_prefix(b"NSpid:"))
        .is_some_and(|line| {
            let mut fields = line
                .split(u8::is_ascii_whitespace)
                .filter(|field| !field.is_empty());
            let pid = fields
                .next()
                .and_then(|field| std::str::from_utf8(field).ok())
                .and_then(|field| field.parse::<i32>().ok());
            pid == Some(own_pid.as_raw()) && fields.next().is_none()
        })
}

fn count_live(proc: &OwnedFd, pids: Option<&[Pid]>, group: i32) -> Option<usize> {
    let mut count = 0;
    let mut buffer = [0; 4096];
    for pid in pids.into_iter().flatten() {
        let path = format!("{}/stat", pid.as_raw());
        let length = match openat(proc, path.as_str(), READ, Mode::empty())
            .and_then(|fd| read(fd, &mut buffer[..]))
        {
            Ok(length) => length,
            Err(Errno::NOENT | Errno::SRCH) => continue,
            Err(_) => return None,
        };
        let (observed_group, live) = stat_group(complete_stat(&buffer, length)?)?;
        if observed_group == group && live {
            count += 1;
        }
    }
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_membership_requires_the_procfs_mounts_pid_namespace() {
        let pid = Pid::from_raw(17);
        assert!(pid_in_own_namespace(b"Name:\t\xff\nNSpid:\t17\n", pid));
        for status in [
            b"NSpid:\t17\t17\n".as_slice(),
            b"NSpid:\t57\t17\n",
            b"NSpid:\t57\n",
            b"NSpid:\t17",
            b"Name:\tx\n",
        ] {
            assert!(!pid_in_own_namespace(status, pid));
        }
    }
}
