//! Cross-runner copies (runner split §9.2): a parent's state pinned as a
//! base snapshot, carried to another runner as a git bundle, and applied
//! there into the tree's mirror repository, from which the copy is minted;
//! and a sealed candidate carried back the same way. Git only: a JJ
//! checkout crosses runners only when it is colocated with git.
//!
//! A bundle is created into the source repository's private bundle
//! directory and read from it in chunks; the receiving side writes the
//! chunks into its own upload file, checks the digest and applies it. The
//! host relays the chunks and stores nothing.

use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use super::{
    SessionVcs, VcsError, VcsSnapshot, detect_vcs, git_query, git_run, git_snapshot, pins,
};

/// A bundle a source runner created, by the id it reads it under.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BundleFacts {
    /// The bundle's id within its repository's bundle directory.
    pub id: String,
    /// The SHA-256 of its bytes, lowercase hex.
    pub digest: String,
    /// How many bytes it holds.
    pub len: u64,
}

/// The most bytes one relay chunk carries.
pub const BUNDLE_CHUNK_BYTES: u64 = 1 << 20;

/// The ref a transfer pins its base snapshot under while it is carried.
#[must_use]
pub fn transfer_ref(namespace: &str, agent: &str) -> String {
    pins::reference(namespace, "transfers", agent)
}

fn require_git(repo: &Path) -> Result<(), VcsError> {
    match detect_vcs(repo)? {
        SessionVcs::Git => Ok(()),
        SessionVcs::Jj => Err(VcsError::mint_unavailable(
            "a copy crosses runners only from a git checkout",
        )),
    }
}

fn transfer_error(detail: impl Into<String>) -> VcsError {
    VcsError::mint_failed(detail)
}

/// The directory `family` (`bundles` or `uploads`) of `repo`'s private
/// transfer files, created owner-only.
fn transfer_dir(repo: &Path, family: &str) -> Result<PathBuf, VcsError> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let dir = pins::common(repo)?.join("plak-transfer").join(family);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|error| transfer_error(format!("transfer directory: {error}")))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| transfer_error(format!("transfer directory: {error}")))?;
    Ok(dir)
}

/// The file a bundle or upload id names; an id is one plain file name.
fn transfer_file(repo: &Path, family: &str, id: &str) -> Result<PathBuf, VcsError> {
    let plain = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !id.starts_with('.');
    if !plain {
        return Err(transfer_error("invalid transfer id"));
    }
    Ok(transfer_dir(repo, family)?.join(id))
}

fn file_digest(path: &Path) -> Result<(String, u64), VcsError> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| transfer_error(format!("bundle could not be read: {error}")))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut len = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| transfer_error(format!("bundle could not be read: {error}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        len += u64::try_from(read).unwrap_or(u64::MAX);
    }
    Ok((
        hasher
            .finalize()
            .iter()
            .fold(String::with_capacity(64), |mut text, byte| {
                use std::fmt::Write as _;
                let _ = write!(text, "{byte:02x}");
                text
            }),
        len,
    ))
}

/// Pins the parent's state (HEAD and its non-secret uncommitted work) as a
/// snapshot commit under [`transfer_ref`], so it survives until carried.
pub(crate) fn snapshot_base(
    parent_cwd: &Path,
    namespace: &str,
    agent: &str,
) -> Result<VcsSnapshot, VcsError> {
    require_git(parent_cwd)?;
    let snapshot = git_snapshot(parent_cwd, agent)?;
    pins::update(
        parent_cwd,
        namespace,
        &[(
            transfer_ref(namespace, agent),
            Some(snapshot.commit.as_str()),
        )],
        false,
    )?;
    Ok(snapshot)
}

/// Bundles `reference` from `repo`, leaving out what the receiver already
/// holds: every commit of `exclude` that `repo` knows.
pub(crate) fn bundle_create(
    repo: &Path,
    reference: &str,
    exclude: &[String],
) -> Result<BundleFacts, VcsError> {
    require_git(repo)?;
    let dir = transfer_dir(repo, "bundles")?;
    let file = tempfile::Builder::new()
        .prefix("b-")
        .suffix(".bundle")
        .tempfile_in(&dir)
        .map_err(|error| transfer_error(format!("bundle file: {error}")))?;
    let (_, path) = file
        .keep()
        .map_err(|error| transfer_error(format!("bundle file: {error}")))?;
    let path_text = path.display().to_string();
    let mut args = vec![
        "bundle".to_owned(),
        "create".to_owned(),
        "-q".to_owned(),
        path_text,
        reference.to_owned(),
    ];
    for commit in exclude {
        let known = git_query(repo, &["cat-file", "-e", &format!("{commit}^{{commit}}")])
            .is_ok_and(|output| output.status.success());
        if known {
            args.push(format!("^{commit}"));
        }
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = git_run(repo, &args).map_err(|error| transfer_error(error.to_string()))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(transfer_error(format!(
            "git bundle create refused: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let (digest, len) = file_digest(&path)?;
    // Named by its digest, so the same bundle created again is the same
    // file under the same id.
    let id = format!("b-{digest}.bundle");
    std::fs::rename(&path, dir.join(&id))
        .map_err(|error| transfer_error(format!("bundle file: {error}")))?;
    Ok(BundleFacts { id, digest, len })
}

/// Up to `len` bytes of bundle `id` from `offset`.
pub(crate) fn bundle_read(
    repo: &Path,
    id: &str,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>, VcsError> {
    let path = transfer_file(repo, "bundles", id)?;
    let mut file = std::fs::File::open(&path)
        .map_err(|error| transfer_error(format!("bundle could not be read: {error}")))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| transfer_error(format!("bundle could not be read: {error}")))?;
    let mut bytes = Vec::new();
    file.take(len.min(BUNDLE_CHUNK_BYTES))
        .read_to_end(&mut bytes)
        .map_err(|error| transfer_error(format!("bundle could not be read: {error}")))?;
    Ok(bytes)
}

/// Removes bundle `id`; an absent one is already gone.
pub(crate) fn bundle_drop(repo: &Path, id: &str) -> Result<(), VcsError> {
    match std::fs::remove_file(transfer_file(repo, "bundles", id)?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(transfer_error(format!(
            "bundle could not be removed: {error}"
        ))),
    }
}

/// Creates `repo` as an empty git repository when nothing is there yet:
/// the tree's mirror on a runner that receives its first copy.
fn ensure_repository(repo: &Path) -> Result<(), VcsError> {
    if repo.join(".git").exists() {
        return Ok(());
    }
    std::fs::create_dir_all(repo)
        .map_err(|error| transfer_error(format!("mirror could not be created: {error}")))?;
    let output =
        git_run(repo, &["init", "-q"]).map_err(|error| transfer_error(error.to_string()))?;
    if !output.status.success() {
        return Err(transfer_error("git init refused the mirror"));
    }
    Ok(())
}

/// Writes `bytes` at `offset` of upload `id` in `repo`, creating the
/// repository when it is new. Writing a chunk again writes the same bytes.
pub(crate) fn bundle_write(
    repo: &Path,
    id: &str,
    offset: u64,
    bytes: &[u8],
) -> Result<(), VcsError> {
    use std::os::unix::fs::OpenOptionsExt as _;
    ensure_repository(repo)?;
    let path = transfer_file(repo, "uploads", id)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|error| transfer_error(format!("upload could not be written: {error}")))?;
    let held = file
        .metadata()
        .map_err(|error| transfer_error(format!("upload could not be written: {error}")))?
        .len();
    if offset > held {
        return Err(transfer_error(
            "an upload chunk arrived ahead of the one before it",
        ));
    }
    file.set_len(offset)
        .and_then(|()| file.seek(SeekFrom::Start(offset)).map(drop))
        .and_then(|()| file.write_all(bytes))
        .and_then(|()| file.sync_all())
        .map_err(|error| transfer_error(format!("upload could not be written: {error}")))
}

/// Applies upload `id` to `repo` once its bytes hash to `digest`: fetches
/// each `(bundle ref, local ref)` of `fetch` (an empty local ref fetches
/// the objects alone, and a named one is a private staging ref, forced),
/// then creates `pins` under `namespace`. A pin is created only where none
/// is: a reference that already names a commit is never moved. The upload
/// is removed whatever the outcome, so a failed apply leaves nothing.
pub(crate) fn bundle_apply(
    repo: &Path,
    id: &str,
    digest: &str,
    namespace: &str,
    fetch: &[(String, String)],
    pins: &[(String, String)],
) -> Result<(), VcsError> {
    require_git(repo)?;
    let path = transfer_file(repo, "uploads", id)?;
    let applied = apply_upload(repo, &path, digest, namespace, fetch, pins);
    let _ = std::fs::remove_file(&path);
    applied
}

fn apply_upload(
    repo: &Path,
    path: &Path,
    digest: &str,
    namespace: &str,
    fetch: &[(String, String)],
    pins: &[(String, String)],
) -> Result<(), VcsError> {
    let (held, _) = file_digest(path)?;
    if held != digest {
        return Err(transfer_error(
            "the carried bundle does not match its digest; nothing was applied",
        ));
    }
    let path_text = path.display().to_string();
    let verified = git_run(repo, &["bundle", "verify", "-q", &path_text])
        .map_err(|error| transfer_error(error.to_string()))?;
    if !verified.status.success() {
        return Err(transfer_error(format!(
            "git bundle verify refused: {}",
            String::from_utf8_lossy(&verified.stderr).trim()
        )));
    }
    let mut args = vec![
        "fetch".to_owned(),
        "-q".to_owned(),
        "--no-tags".to_owned(),
        "--no-write-fetch-head".to_owned(),
        path_text,
    ];
    args.extend(fetch.iter().map(|(from, to)| {
        if to.is_empty() {
            from.clone()
        } else {
            format!("+{from}:{to}")
        }
    }));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let fetched = git_run(repo, &args).map_err(|error| transfer_error(error.to_string()))?;
    if !fetched.status.success() {
        return Err(transfer_error(format!(
            "git fetch from the bundle refused: {}",
            String::from_utf8_lossy(&fetched.stderr).trim()
        )));
    }
    if !pins.is_empty() {
        let refs: Vec<(String, Option<&str>)> = pins
            .iter()
            .map(|(name, commit)| (name.clone(), Some(commit.as_str())))
            .collect();
        pins::update(repo, namespace, &refs, true)?;
    }
    Ok(())
}

/// Removes `repo`'s bundle and upload files older than `max_age`: what a
/// carry that never finished left behind. A carry's rerun writes its files
/// again. Returns how many it removed.
pub(crate) fn sweep(repo: &Path, max_age: std::time::Duration) -> Result<u32, VcsError> {
    if !repo.exists() || require_git(repo).is_err() {
        return Ok(0);
    }
    let root = pins::common(repo)?.join("plak-transfer");
    let mut removed = 0;
    for family in ["bundles", "uploads"] {
        let Ok(entries) = std::fs::read_dir(root.join(family)) else {
            continue;
        };
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= max_age);
            if stale && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// The commit `reference` names in `repo`, if it names one.
pub(crate) fn ref_commit(repo: &Path, reference: &str) -> Result<Option<String>, VcsError> {
    if !repo.exists() {
        return Ok(None);
    }
    let output = git_query(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reference}^{{commit}}"),
        ],
    )
    .map_err(|error| transfer_error(error.to_string()))?;
    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )),
        Some(1) => Ok(None),
        _ => Err(transfer_error("git rev-parse could not read the reference")),
    }
}

#[cfg(test)]
mod tests;
