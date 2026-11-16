//! Retained-directory, no-follow reads of private secret files.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat, fstat, open, openat, statat};

use super::{MAX_NAMED_SECRET_BYTES, NamedSecret, PlakPaths};

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

fn directory_valid(stat: &Stat) -> bool {
    FileType::from_raw_mode(stat.st_mode) == FileType::Directory
        && stat.st_uid == rustix::process::geteuid().as_raw()
        && stat.st_mode & 0o777 == 0o700
}

fn file_valid(stat: &Stat) -> bool {
    FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
        && stat.st_uid == rustix::process::geteuid().as_raw()
        && stat.st_mode & 0o477 == 0o400
        && stat.st_nlink == 1
}

fn same_identity(before: &Stat, after: &Stat) -> bool {
    before.st_dev == after.st_dev && before.st_ino == after.st_ino
}

pub(super) fn directory(paths: &PlakPaths, path: &Path) -> Result<Option<File>, String> {
    let metadata = match fs::symlink_metadata(paths.home()) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("could not inspect the private secrets home".into()),
    };
    let home = open(paths.home(), DIRECTORY_FLAGS, Mode::empty())
        .map(File::from)
        .map_err(|_| "could not open the private secrets home")?;
    let retained = home
        .metadata()
        .map_err(|_| "could not inspect the private secrets home")?;
    if !metadata.file_type().is_dir()
        || metadata.dev() != retained.dev()
        || metadata.ino() != retained.ino()
        || !directory_valid(
            &fstat(&home).map_err(|_| "could not inspect the private secrets home")?,
        )
    {
        return Err("secrets home is not a retained private directory".into());
    }
    let relative = path
        .strip_prefix(paths.home())
        .map_err(|_| "secret directory is outside its home")?;
    let mut directory = home;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err("unsafe secret directory component".into());
        };
        let before = match statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(_) => return Err("could not inspect the secrets directory".into()),
        };
        if !directory_valid(&before) {
            return Err(
                "secrets directory is not private to the current user; run chmod 700".into(),
            );
        }
        let child = openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())
            .map(File::from)
            .map_err(|_| "could not retain the secrets directory")?;
        let after =
            fstat(&child).map_err(|_| "could not inspect the retained secrets directory")?;
        if !directory_valid(&after) || !same_identity(&before, &after) {
            return Err("secrets directory changed while it was opened".into());
        }
        directory = child;
    }
    Ok(Some(directory))
}

pub(super) fn read(directory: &File, name: &OsStr) -> Result<Option<NamedSecret>, String> {
    read_with_hook(directory, name, || {})
}

fn read_with_hook(
    directory: &File,
    name: &OsStr,
    before_open: impl FnOnce(),
) -> Result<Option<NamedSecret>, String> {
    let before = match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err("could not inspect the secret".into()),
    };
    if !file_valid(&before) {
        return Err(
            "secret is not a private single-link file of the current user; run chmod 600".into(),
        );
    }
    before_open();
    let file = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| "could not open the private secret")?;
    let after = fstat(&file).map_err(|_| "could not inspect the opened secret")?;
    if !file_valid(&after) || !same_identity(&before, &after) {
        return Err("secret changed while it was opened".into());
    }
    let mut bytes = zeroize::Zeroizing::new(Vec::with_capacity(MAX_NAMED_SECRET_BYTES + 1));
    file.take((MAX_NAMED_SECRET_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "could not read the private secret")?;
    if bytes.len() > MAX_NAMED_SECRET_BYTES {
        return Err("secret is too large".into());
    }
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| "secret is not UTF-8")?
        .trim();
    Ok((!value.is_empty()).then(|| NamedSecret(zeroize::Zeroizing::new(value.to_owned()))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _, symlink};

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.path().join("key.secret"))
            .unwrap();
        root
    }

    #[test]
    fn replacement_and_same_inode_symlink_races_refuse_before_reading() {
        for symlink_to_original in [false, true] {
            let root = fixture();
            let directory = File::open(root.path()).unwrap();
            let result = read_with_hook(&directory, OsStr::new("key.secret"), || {
                fs::rename(
                    root.path().join("key.secret"),
                    root.path().join("original.secret"),
                )
                .unwrap();
                if symlink_to_original {
                    symlink("original.secret", root.path().join("key.secret")).unwrap();
                } else {
                    fs::copy(
                        root.path().join("original.secret"),
                        root.path().join("key.secret"),
                    )
                    .unwrap();
                }
            });
            assert!(result.is_err());
        }
    }

    #[test]
    fn another_owner_is_never_a_private_secret() {
        let root = fixture();
        let file = File::open(root.path().join("key.secret")).unwrap();
        let mut metadata = fstat(&file).unwrap();
        assert!(file_valid(&metadata));
        metadata.st_uid = metadata.st_uid.wrapping_add(1);
        assert!(!file_valid(&metadata));
    }

    #[test]
    fn opened_file_permissions_and_link_count_are_revalidated() {
        for hard_link in [false, true] {
            let root = fixture();
            let directory = File::open(root.path()).unwrap();
            let result = read_with_hook(&directory, OsStr::new("key.secret"), || {
                if hard_link {
                    fs::hard_link(
                        root.path().join("key.secret"),
                        root.path().join("alias.secret"),
                    )
                    .unwrap();
                } else {
                    fs::set_permissions(
                        root.path().join("key.secret"),
                        fs::Permissions::from_mode(0o644),
                    )
                    .unwrap();
                }
            });
            assert!(result.is_err());
        }
    }
}
