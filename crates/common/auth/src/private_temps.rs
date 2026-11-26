//! Bounded reclamation of abandoned private publication files.

use std::os::fd::AsFd;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat};

const MAX_ENTRIES: usize = 4096;
const MAX_REMOVALS: usize = 64;
const MIN_AGE_SECS: i64 = 24 * 60 * 60;

/// Best-effort cleanup beside a private atomic writer. Only its reserved
/// staging names should match, never a published filename.
pub fn sweep(directory: &Path, matches: impl Fn(&str) -> bool) {
    let Ok(directory) = rustix::fs::open(
        directory,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return;
    };
    sweep_at(&directory, matches);
}

/// Same sweep with the directory already open.
pub fn sweep_at(directory: &impl AsFd, matches: impl Fn(&str) -> bool) {
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return;
    };
    let Ok(now) = i64::try_from(now.as_secs()) else {
        return;
    };
    sweep_before(directory, matches, now - MIN_AGE_SECS);
}

fn sweep_before(directory: &impl AsFd, matches: impl Fn(&str) -> bool, cutoff: i64) {
    let uid = rustix::process::geteuid().as_raw();
    let Ok(parent) = rustix::fs::fstat(directory) else {
        return;
    };
    if FileType::from_raw_mode(parent.st_mode) != FileType::Directory
        || parent.st_uid != uid
        || parent.st_mode & 0o022 != 0
    {
        return;
    }
    let Ok(entries) = Dir::read_from(directory) else {
        return;
    };
    let mut removed = 0;
    for entry in entries.take(MAX_ENTRIES).map_while(Result::ok) {
        let name = entry.file_name();
        if !name.to_str().is_ok_and(&matches) {
            continue;
        }
        let flags = AtFlags::SYMLINK_NOFOLLOW;
        let Ok(stat) = rustix::fs::statat(directory, name, flags) else {
            continue;
        };
        if !eligible(&stat, uid, cutoff) {
            continue;
        }
        if rustix::fs::unlinkat(directory, name, AtFlags::empty()).is_ok() {
            removed += 1;
            if removed == MAX_REMOVALS {
                break;
            }
        }
    }
}

fn eligible(stat: &Stat, uid: u32, cutoff: i64) -> bool {
    FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
        && stat.st_uid == uid
        && !Mode::from_raw_mode(stat.st_mode).intersects(Mode::RWXG | Mode::RWXO)
        && stat.st_mtime < cutoff
        && stat.st_ctime < cutoff
}

/// Sweeps a target's dot-prefixed, 32-lowercase-hex temporary siblings.
pub fn sweep_hex_siblings(target: &Path) {
    let Some((directory, base)) = target
        .parent()
        .zip(target.file_name().and_then(|n| n.to_str()))
    else {
        return;
    };
    let prefix = format!(".{base}.");
    sweep(directory, |name| {
        name.strip_prefix(&prefix)
            .and_then(|suffix| suffix.strip_suffix(".tmp"))
            .is_some_and(|hex| {
                hex.len() == 32
                    && hex
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _, symlink};

    #[test]
    fn sweeps_only_old_private_files_in_the_reserved_namespace() -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path();
        let directory = File::open(path)?;
        for name in [".stage.old", "published"] {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path.join(name))?;
        }
        let public = File::create(path.join(".stage.public"))?;
        public.set_permissions(fs::Permissions::from_mode(0o644))?;
        symlink(path.join("published"), path.join(".stage.link"))?;
        fs::create_dir(path.join(".stage.dir"))?;
        let is_stage = |name: &str| name.starts_with(".stage.");
        sweep_at(&directory, is_stage);
        assert!(path.join(".stage.old").is_file(), "a fresh write must stay");

        sweep_before(&directory, is_stage, i64::MAX);
        assert!(!path.join(".stage.old").exists());
        for name in [".stage.public", ".stage.link", ".stage.dir", "published"] {
            assert!(path.join(name).exists(), "{name}");
        }
        Ok(())
    }

    #[test]
    fn group_writable_parent_blocks_sweep_and_fresh_writer_can_publish() -> std::io::Result<()> {
        use std::io::Write as _;
        let root = tempfile::tempdir()?;
        let directory = File::open(root.path())?;
        let stage = root.path().join(".stage.writer");
        let mut writer = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&stage)?;
        directory.set_permissions(fs::Permissions::from_mode(0o770))?;
        sweep_before(&directory, |name| name == ".stage.writer", i64::MAX);
        assert!(
            stage.is_file(),
            "even old-looking files in a shared directory must stay"
        );
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
        std::thread::scope(|scope| {
            scope.spawn(|| sweep_at(&directory, |name| name == ".stage.writer"));
            writer.write_all(b"new complete value")
        })?;
        let published = root.path().join("published");
        fs::rename(stage, &published)?;
        assert_eq!(fs::read(published)?, b"new complete value");
        Ok(())
    }
}
