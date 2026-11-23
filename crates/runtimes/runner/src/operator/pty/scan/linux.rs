use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::Identity;

const MAX_ENTRIES: usize = 65_536;
const STAT_BOUND: usize = 65_536;

pub(super) fn capture(pid: u32) -> Result<Identity, ()> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    read_stat(&path, pid, &mut [0; STAT_BOUND]).and_then(|row| row.ok_or(()))
}

pub(super) fn scan() -> Result<Vec<Identity>, ()> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut path = PathBuf::from("/proc");
    let mut buffer = [0; STAT_BOUND];
    let mut rows = Vec::new();
    for (index, entry) in std::fs::read_dir(&path).map_err(|_| ())?.enumerate() {
        if index > MAX_ENTRIES || Instant::now() > deadline {
            return Err(());
        }
        let name = entry.map_err(|_| ())?.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        path.push(&name);
        path.push("stat");
        rows.extend(read_stat(&path, pid, &mut buffer)?);
        path.pop();
        path.pop();
    }
    (Instant::now() <= deadline).then_some(rows).ok_or(())
}

fn read_stat(path: &Path, pid: u32, buffer: &mut [u8]) -> Result<Option<Identity>, ()> {
    // Procfs produces a single short record. Read it once, without a size
    // probe or a heap buffer per process; never infer absence from a short row.
    match std::fs::File::open(path).and_then(|mut file| file.read(buffer)) {
        Ok(0) => Ok(None), // The task vanished between open and read.
        Ok(length) if length < buffer.len() => parse_stat(pid, &buffer[..length]).map(Some),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
        {
            Ok(None)
        }
        _ => Err(()),
    }
}

fn parse_stat(pid: u32, bytes: &[u8]) -> Result<Identity, ()> {
    // The command name can contain spaces, closing parentheses and
    // non-UTF-8 bytes. Only the numeric tail needs decoding.
    let (head, _) = bytes.split_at(bytes.iter().position(|&b| b == b' ').ok_or(())?);
    if std::str::from_utf8(head)
        .map_err(|_| ())?
        .parse::<u32>()
        .ok()
        != Some(pid)
    {
        return Err(());
    }
    let tail = bytes.rsplit(|&b| b == b')').next().ok_or(())?;
    if tail.len() == bytes.len() {
        return Err(());
    }
    let tail = tail
        .strip_prefix(b" ")
        .and_then(|s| s.strip_suffix(b"\n"))
        .ok_or(())?;
    let mut fields = std::str::from_utf8(tail)
        .map_err(|_| ())?
        .split_ascii_whitespace();
    let state = fields.next().filter(|state| state.len() == 1).ok_or(())?;
    let parent = fields.next().ok_or(())?.parse().map_err(|_| ())?;
    let group = fields.next().ok_or(())?.parse().map_err(|_| ())?;
    let session = fields.next().ok_or(())?;
    let started = fields.nth(15).ok_or(())?;
    if session.parse::<u32>().is_err() || started.parse::<u64>().is_err() {
        return Err(());
    }
    Ok(Identity {
        pid,
        parent,
        group,
        session: session.into(),
        started: started.into(),
        zombie: state == "Z",
        anchor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_evidence_requires_a_complete_row_for_the_requested_pid() {
        let bytes = b"42 (name ) \xff\n) Z 7 42 42 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 911 0\n";
        let Ok(row) = parse_stat(42, bytes) else {
            panic!("the complete stat row should be accepted");
        };
        assert_eq!((row.pid, row.parent, row.group), (42, 7, 42));
        assert_eq!(
            (&*row.session, &*row.started, row.zombie),
            ("42", "911", true)
        );
        assert!(parse_stat(7, bytes).is_err());
        assert!(parse_stat(42, &bytes[..bytes.len() - 1]).is_err());
        assert!(parse_stat(42, b"42 ()) Z 7 42 42\n").is_err());
    }
}
