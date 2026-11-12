use std::fs::{self, File, Metadata};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use plakous_model::{Digest, DigestDomain, sha256_hex};
use rustix::fs::{Mode, OFlags};

use super::process::{Executor, Output};
use super::{ChangesContent, ChangesRefusal, ChangesRepository, MAX_CHANGES_INPUT_BYTES};
use crate::environment::backend::{ExecutionWorkspace, workspace};

static FILESYSTEM_LANES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);

#[derive(Clone)]
pub(super) struct Repository {
    pub identity: ChangesRepository,
    pub attributes: String,
    pub executor: Executor,
    protected: Vec<PathBuf>,
}

pub(super) async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, ChangesRefusal> + Send + 'static,
) -> Result<T, ChangesRefusal> {
    let permit = FILESYSTEM_LANES
        .acquire()
        .await
        .map_err(|_| ChangesRefusal::Io)?;
    tokio::task::spawn_blocking(move || {
        // A cancelled request must not release capacity while an OS filesystem
        // call is still blocked in this detached worker.
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|_| ChangesRefusal::Io)?
}

impl Repository {
    pub async fn discover(
        workspace: ExecutionWorkspace,
        known: Option<PathBuf>,
        executor: Executor,
        protected: Vec<PathBuf>,
    ) -> Result<Self, ChangesRefusal> {
        let copy = workspace.clone();
        let excluded = protected.clone();
        let identity = blocking(move || {
            workspace::verified_workspace_root(&copy).map_err(|_| ChangesRefusal::Conflict)?;
            identify(copy.root(), known.as_deref(), &excluded)
        })
        .await?;
        let mut repo = Self {
            identity,
            attributes: String::new(),
            executor,
            protected,
        };
        // Discovery is bounded to the explicitly verified checkout. No climb
        // from a subdirectory into an ambient enclosing home repository.
        let paths = repo
            .executor
            .run(
                &repo.identity.toplevel,
                Some(&repo.identity),
                None,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--absolute-git-dir",
                    "--show-toplevel",
                    "--git-common-dir",
                ],
                16 * 1024,
            )
            .await?;
        if !paths.success {
            return Err(ChangesRefusal::UnsafeRepository);
        }
        let paths = String::from_utf8(paths.bytes).map_err(|_| ChangesRefusal::UnsafeRepository)?;
        let expected = [
            &repo.identity.git_dir,
            &repo.identity.toplevel,
            &repo.identity.common_dir,
        ];
        if paths
            .lines()
            .map(Path::new)
            .ne(expected.into_iter().map(PathBuf::as_path))
        {
            return Err(ChangesRefusal::UnsafeRepository);
        }
        let worktree = repo
            .query(&["config", "--path", "--get", "core.worktree"], 4096)
            .await?;
        if worktree.success {
            let path = text(worktree.bytes)?;
            let resolved = repo
                .identity
                .git_dir
                .join(path)
                .canonicalize()
                .map_err(|_| ChangesRefusal::UnsafeRepository)?;
            if resolved != repo.identity.toplevel {
                return Err(ChangesRefusal::UnsafeRepository);
            }
        }
        repo.attributes = text(
            repo.query(&["hash-object", "-t", "tree", "--stdin"], 128)
                .await?
                .bytes,
        )?;
        if !super::types::object(&repo.attributes) {
            return Err(ChangesRefusal::VcsUnavailable);
        }
        Ok(repo)
    }

    pub async fn query(&self, args: &[&str], cap: usize) -> Result<Output, ChangesRefusal> {
        self.executor.check()?;
        let identity = self.identity.clone();
        let protected = self.protected.clone();
        blocking(move || {
            let current = identify(&identity.workspace, Some(&identity.toplevel), &protected)?;
            if current != identity {
                return Err(ChangesRefusal::Conflict);
            }
            Ok(())
        })
        .await?;
        let attrs = (!self.attributes.is_empty()).then_some(self.attributes.as_str());
        let top = self
            .executor
            .run(
                &self.identity.toplevel,
                Some(&self.identity),
                attrs,
                &["rev-parse", "--show-toplevel"],
                4096,
            )
            .await?;
        if !top.success || Path::new(&text(top.bytes)?) != self.identity.toplevel {
            return Err(ChangesRefusal::Conflict);
        }
        let worktree = self
            .executor
            .run(
                &self.identity.toplevel,
                Some(&self.identity),
                attrs,
                &["config", "--path", "--get", "core.worktree"],
                4096,
            )
            .await?;
        if worktree.success {
            let configured = self.identity.git_dir.join(text(worktree.bytes)?);
            let expected = self.identity.toplevel.clone();
            blocking(move || {
                if canonical(&configured)? != expected {
                    return Err(ChangesRefusal::UnsafeRepository);
                }
                Ok(())
            })
            .await?;
        }
        self.executor
            .run(
                &self.identity.toplevel,
                Some(&self.identity),
                attrs,
                args,
                cap,
            )
            .await
    }

    pub fn pathspec(&self, path: Option<&str>) -> Result<String, ChangesRefusal> {
        let prefix = self
            .identity
            .workspace
            .strip_prefix(&self.identity.toplevel)
            .map_err(|_| ChangesRefusal::UnsafeRepository)?;
        let relative = path.map_or_else(|| prefix.to_path_buf(), |path| prefix.join(path));
        Ok(relative
            .to_str()
            .ok_or(ChangesRefusal::UnsafeRepository)?
            .to_owned())
    }

    pub async fn index_identity(&self) -> Result<String, ChangesRefusal> {
        let path = self.identity.git_dir.join("index");
        let control = self.executor.clone();
        blocking(move || {
            control.check()?;
            match open_regular(&path) {
                Ok(file) => hash(file, 16 * 1024 * 1024),
                Err(ChangesRefusal::NotARepository) => Ok(sha256_hex(&[b"absent index"])),
                Err(error) => Err(error),
            }
        })
        .await
    }
}

pub(super) async fn content(
    workspace: &ExecutionWorkspace,
    path: &str,
) -> Result<ChangesContent, ChangesRefusal> {
    super::types::relative(path)?;
    let workspace = workspace.clone();
    let path = path.to_owned();
    blocking(move || {
        let file = match workspace::open_workspace_beneath(
            &workspace,
            Path::new(&path),
            OFlags::RDONLY | OFlags::NONBLOCK,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == crate::ExecutionBackendErrorKind::NotFound => {
                return Ok(ChangesContent::Missing);
            }
            Err(_) => return Ok(ChangesContent::Unsupported),
        };
        let metadata = file.metadata().map_err(|_| ChangesRefusal::Io)?;
        if !metadata.is_file() {
            return Ok(ChangesContent::Unsupported);
        }
        let size = metadata.len();
        if size > MAX_CHANGES_INPUT_BYTES {
            return Ok(ChangesContent::TooLarge { size });
        }
        Ok(ChangesContent::File {
            size,
            digest: sha256_hex(&[
                b"git-worktree-input\0",
                hash(file, MAX_CHANGES_INPUT_BYTES)?.as_bytes(),
                &metadata.mode().to_be_bytes(),
            ]),
        })
    })
    .await
}

pub(super) fn identify(
    workspace: &Path,
    known: Option<&Path>,
    protected: &[PathBuf],
) -> Result<ChangesRepository, ChangesRefusal> {
    identify_owned(
        workspace,
        known,
        protected,
        rustix::process::geteuid().as_raw(),
    )
}

pub(super) fn identify_owned(
    workspace: &Path,
    known: Option<&Path>,
    protected: &[PathBuf],
    owner: u32,
) -> Result<ChangesRepository, ChangesRefusal> {
    let workspace = canonical(workspace)?;
    let toplevel = canonical(known.unwrap_or(&workspace))?;
    if !workspace.starts_with(&toplevel) {
        return Err(ChangesRefusal::UnsafeRepository);
    }
    for path in [&workspace, &toplevel] {
        exclude(path, protected)?;
    }
    let marker = toplevel.join(".git");
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(
                if workspace.join(".jj").symlink_metadata().is_ok()
                    || toplevel.join(".jj").symlink_metadata().is_ok()
                {
                    ChangesRefusal::VcsUnsupported
                } else {
                    ChangesRefusal::NotARepository
                },
            );
        }
        Err(_) => return Err(ChangesRefusal::Io),
    };
    let git_dir = if metadata.is_dir() {
        canonical(&marker)?
    } else if metadata.is_file() {
        let redirect = read_small(&marker)?;
        let target = redirect
            .trim_end()
            .strip_prefix("gitdir: ")
            .filter(|path| !path.contains(['\n', '\r']))
            .ok_or(ChangesRefusal::UnsafeRepository)?;
        canonical(&toplevel.join(target))?
    } else {
        return Err(ChangesRefusal::UnsafeRepository);
    };
    exclude(&git_dir, protected)?;
    let common_dir = match fs::symlink_metadata(git_dir.join("commondir")) {
        Ok(_) => canonical(&git_dir.join(read_small(&git_dir.join("commondir"))?.trim_end()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
        Err(_) => return Err(ChangesRefusal::UnsafeRepository),
    };
    exclude(&common_dir, protected)?;
    for root in [&git_dir, &common_dir] {
        for name in [
            "config",
            "config.worktree",
            "HEAD",
            "index",
            "info",
            "objects",
            "objects/info",
            "objects/pack",
            "packed-refs",
            "refs",
        ] {
            if fs::symlink_metadata(root.join(name))
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(ChangesRefusal::UnsafeRepository);
            }
        }
        // Object alternates are another repository redirect and may hide a
        // protected common directory. This viewer does not follow them.
        if fs::symlink_metadata(root.join("objects/info/alternates")).is_ok() {
            return Err(ChangesRefusal::UnsafeRepository);
        }
        match fs::symlink_metadata(root.join("info/attributes")) {
            Ok(metadata) if metadata.is_file() && metadata.len() == 0 => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) | Err(_) => return Err(ChangesRefusal::UnsafeRepository),
        }
    }
    let identity = [&workspace, &toplevel, &git_dir, &common_dir]
        .into_iter()
        .map(|path| {
            let metadata = fs::metadata(path).map_err(|_| ChangesRefusal::UnsafeRepository)?;
            if metadata.uid() != owner {
                return Err(ChangesRefusal::UnsafeRepository);
            }
            Ok(format!(
                "{}\0{}\0{}",
                path.display(),
                metadata.dev(),
                metadata.ino()
            ))
        })
        .collect::<Result<Vec<_>, ChangesRefusal>>()?
        .join("\0");
    Ok(ChangesRepository {
        git_view: workspace.join(".jj").symlink_metadata().is_ok()
            || toplevel.join(".jj").symlink_metadata().is_ok(),
        workspace,
        toplevel,
        git_dir,
        common_dir,
        identity: sha256_hex(&[identity.as_bytes()]),
    })
}

fn canonical(path: &Path) -> Result<PathBuf, ChangesRefusal> {
    let resolved = path
        .canonicalize()
        .map_err(|_| ChangesRefusal::UnsafeRepository)?;
    // Dot components in git worktree redirects are lexical, not symlinks.
    let mut lexical = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                lexical.pop();
            }
            std::path::Component::CurDir => {}
            _ => lexical.push(part),
        }
    }
    if resolved != lexical
        || resolved
            .to_str()
            .is_none_or(|path| path.contains(['\n', '\r']))
    {
        return Err(ChangesRefusal::UnsafeRepository);
    }
    Ok(resolved)
}

pub(super) fn protected_roots_from(
    home: Option<&std::ffi::OsStr>,
    product: Option<&std::ffi::OsStr>,
    state: Option<&std::ffi::OsStr>,
) -> Result<Vec<PathBuf>, ChangesRefusal> {
    let home = home.filter(|value| !value.is_empty()).map(PathBuf::from);
    let product = product
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".plak")));
    let state = state
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".local/state")))
        .map(|state| state.join("plak"));
    let mut roots = vec![
        product.ok_or(ChangesRefusal::UnsafeRepository)?,
        state.ok_or(ChangesRefusal::UnsafeRepository)?,
    ];
    if let Some(home) = home {
        let default = home.join(".plak");
        if !roots.contains(&default) {
            roots.push(default);
        }
    }
    roots
        .into_iter()
        .map(|path| resolve_missing(&path))
        .collect()
}

fn resolve_missing(path: &Path) -> Result<PathBuf, ChangesRefusal> {
    if !path.is_absolute() {
        return Err(ChangesRefusal::UnsafeRepository);
    }
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(ChangesRefusal::UnsafeRepository)?;
            Ok(resolve_missing(parent)?
                .join(path.file_name().ok_or(ChangesRefusal::UnsafeRepository)?))
        }
        Err(_) => Err(ChangesRefusal::UnsafeRepository),
    }
}

fn exclude(path: &Path, protected: &[PathBuf]) -> Result<(), ChangesRefusal> {
    if protected
        .iter()
        .any(|root| path.starts_with(root) || root.starts_with(path))
    {
        Err(ChangesRefusal::ProtectedPath)
    } else {
        Ok(())
    }
}

fn open_regular(path: &Path) -> Result<File, ChangesRefusal> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        if error == rustix::io::Errno::NOENT {
            ChangesRefusal::NotARepository
        } else {
            ChangesRefusal::UnsafeRepository
        }
    })?;
    let file = File::from(fd);
    if !file.metadata().map_err(|_| ChangesRefusal::Io)?.is_file() {
        return Err(ChangesRefusal::UnsafeRepository);
    }
    Ok(file)
}

fn read_small(path: &Path) -> Result<String, ChangesRefusal> {
    let mut bytes = Vec::new();
    open_regular(path)?
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| ChangesRefusal::Io)?;
    if bytes.len() > 4096 {
        return Err(ChangesRefusal::UnsafeRepository);
    }
    String::from_utf8(bytes).map_err(|_| ChangesRefusal::UnsafeRepository)
}

fn stat(metadata: &Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

fn hash(mut file: File, cap: u64) -> Result<String, ChangesRefusal> {
    let before = file.metadata().map_err(|_| ChangesRefusal::Io)?;
    if before.len() > cap {
        return Err(ChangesRefusal::TooLarge);
    }
    let digest = Digest::sha256_reader(DigestDomain::Blob, 1, before.len(), &mut file)
        .map_err(|_| ChangesRefusal::Conflict)?;
    if stat(&before) != stat(&file.metadata().map_err(|_| ChangesRefusal::Io)?) {
        return Err(ChangesRefusal::Conflict);
    }
    Ok(digest.to_string())
}

pub(super) fn text(bytes: Vec<u8>) -> Result<String, ChangesRefusal> {
    String::from_utf8(bytes)
        .map(|text| text.trim_end_matches('\n').to_owned())
        .map_err(|_| ChangesRefusal::UnsafeRepository)
}
