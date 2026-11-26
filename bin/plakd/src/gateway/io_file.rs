//! Private files and directories for the gateway home. Mode, ownership, link
//! count and symlink refusal are checked on the opened descriptor.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags, fstat, open};

pub(crate) fn ensure_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path)?;
    let meta = fs::symlink_metadata(path)?;
    let euid = rustix::process::geteuid().as_raw();
    if !meta.file_type().is_dir() || meta.uid() != euid || meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} must be a private directory", path.display()),
        ));
    }
    Ok(())
}

pub(crate) fn private_file(path: &Path) -> io::Result<File> {
    let fd = open(
        path,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(io::Error::from)?;
    let file = File::from(fd);
    let stat = fstat(&file).map_err(io::Error::from)?;
    let euid = rustix::process::geteuid().as_raw();
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != euid
        || stat.st_mode & 0o077 != 0
        || stat.st_nlink != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be a single-link regular file owned by the current user with mode 0600",
                path.display()
            ),
        ));
    }
    Ok(file)
}

/// Atomically replaces `path` with `bytes` at mode 0600 via a randomized
/// exclusive temporary file (`O_CREAT | O_EXCL | O_NOFOLLOW`), syncing both
/// the file and its parent directory.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        ensure_private_dir(parent)?;
    }
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to replace symlink {}", path.display()),
        ));
    }
    plakous::auth::private_temps::sweep_hex_siblings(path);
    let tmp = random_temp_path(path)?;
    let write_result = (|| -> io::Result<()> {
        let fd = open(
            &tmp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(io::Error::from)?;
        let mut file = File::from(fd);
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    let sync_dir = parent.unwrap_or_else(|| Path::new("."));
    let dir_fd = open(
        sync_dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    File::from(dir_fd).sync_all()?;
    Ok(())
}

fn random_temp_path(path: &Path) -> io::Result<PathBuf> {
    let mut entropy = [0_u8; 16];
    getrandom::fill(&mut entropy).map_err(|error| io::Error::other(error.to_string()))?;
    let mut suffix = String::with_capacity(entropy.len() * 2);
    for byte in entropy {
        use std::fmt::Write as _;
        let _ = write!(suffix, "{byte:02x}");
    }
    let base = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("private");
    Ok(path.with_file_name(format!(".{base}.{suffix}.tmp")))
}
