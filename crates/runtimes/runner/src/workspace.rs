//! Authenticated workspace picker reads; filesystem authority stays on the runner.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

/// Effective workspace consent advertised by the serving daemon.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOptions {
    /// Canonical admitted roots; `None` means unrestricted local admission.
    pub roots: Option<Vec<String>>,
    /// First root, or the explicitly supplied local startup workspace.
    pub default_workspace: Option<String>,
    /// Whether this runner accepts transferred copies.
    pub copies: bool,
}

/// Display-only HEAD metadata. No Git executable, hooks or helpers are run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryHead {
    /// A named branch with a resolved commit.
    Branch {
        /// Git branch name.
        name: String,
    },
    /// A directly named commit.
    Detached,
    /// A named branch with no commit yet.
    Unborn {
        /// Git branch name.
        name: String,
    },
    /// Metadata is unreadable, malformed or outside admitted roots.
    Unavailable,
}

/// Bounded local repository metadata, never a remote URL.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryMetadata {
    /// Checkout directory basename.
    pub name: String,
    /// Closed HEAD state.
    pub head: RepositoryHead,
    /// Full commit object name, absent for unborn/unavailable HEAD.
    pub commit: Option<String>,
}

/// One canonical checkout discovered on this runner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveredWorkspace {
    /// Canonical UTF-8 checkout path.
    pub path: String,
    /// Repository display metadata, when identified.
    pub repo: Option<RepositoryMetadata>,
}

/// A bounded, deterministically ordered discovery snapshot.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDiscovery {
    /// At most 200 distinct checkouts.
    pub workspaces: Vec<DiscoveredWorkspace>,
    /// A count, traversal or deadline bound was reached.
    pub truncated: bool,
}

/// Workspace admission failure, not a transport failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceRefusal {
    /// Not absolute UTF-8 syntax, or contains parent traversal.
    InvalidPath,
    /// Valid but not byte-identical to its current canonical spelling.
    NotCanonical {
        /// Canonical path to resubmit.
        canonical: String,
    },
    /// Admitted path does not exist.
    NotFound,
    /// Admitted path is not a directory.
    NotDirectory,
    /// Lexical or resolved path lies outside consent roots.
    OutsideRoots,
    /// Path is in or contains the product home.
    ProductHome,
    /// Filesystem permissions prevent validation.
    PermissionDenied,
}

/// Runner-owned policy and five-second discovery cache. Local discovery bases
/// are explicit and do not restrict local workspace admission.
pub struct WorkspaceCatalog {
    roots: Option<Vec<PathBuf>>,
    bases: Vec<PathBuf>,
    excluded: PathBuf,
    default: Option<String>,
    copies: bool,
    cache: Mutex<Option<(Instant, WorkspaceDiscovery)>>,
}

impl WorkspaceCatalog {
    /// Freezes canonical machine roots and the protected product home.
    ///
    /// # Errors
    /// Refuses missing, non-directory, non-UTF-8 or protected roots.
    pub fn machine(
        roots: Vec<PathBuf>,
        excluded: &Path,
        copies: bool,
    ) -> Result<Self, WorkspaceRefusal> {
        let excluded = canonical(excluded)?;
        let roots = roots
            .into_iter()
            .map(|path| canonical(&path))
            .collect::<Result<Vec<_>, _>>()?;
        for root in &roots {
            if overlaps(root, &excluded) {
                return Err(WorkspaceRefusal::ProductHome);
            }
            if !root.is_dir() {
                return Err(WorkspaceRefusal::NotDirectory);
            }
            utf8(root)?;
        }
        Ok(Self {
            default: roots.first().map(|p| p.to_string_lossy().into_owned()),
            bases: roots.clone(),
            roots: Some(roots),
            excluded,
            copies,
            cache: Mutex::new(None),
        })
    }

    /// Local admission remains unrestricted outside the product home. Discovery
    /// scans only startup plus recent-workspace parents; never `/`.
    ///
    /// # Errors
    /// Refuses an invalid startup workspace or protected home.
    pub fn local(
        startup: &Path,
        recent: &[PathBuf],
        excluded: &Path,
    ) -> Result<Self, WorkspaceRefusal> {
        let excluded = canonical(excluded)?;
        let startup = canonical(startup)?;
        let mut bases = vec![startup.clone()];
        for path in recent.iter().take(20) {
            if let Some(parent) = path.parent().filter(|p| p.parent().is_some())
                && let Ok(parent) = canonical(parent)
                && !bases.contains(&parent)
            {
                bases.push(parent);
            }
        }
        bases.retain(|base| base.parent().is_some() && !overlaps(base, &excluded));
        let catalog = Self {
            roots: None,
            bases,
            excluded,
            default: Some(utf8(&startup)?),
            copies: false,
            cache: Mutex::new(None),
        };
        catalog.validate(&utf8(&startup)?, true)?;
        Ok(catalog)
    }

    /// Effective daemon roots, without hosts, environment or secrets.
    #[must_use]
    pub fn options(&self) -> WorkspaceOptions {
        WorkspaceOptions {
            roots: self.roots.as_ref().map(|roots| {
                roots
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect()
            }),
            default_workspace: self.default.clone(),
            copies: self.copies,
        }
    }

    /// Fresh validation; containment precedes existence, including missing
    /// symlink targets. `require_canonical` is true at creation/open.
    ///
    /// # Errors
    /// Returns a typed refusal and never changes the filesystem.
    pub fn validate(
        &self,
        path: &str,
        require_canonical: bool,
    ) -> Result<String, WorkspaceRefusal> {
        let path = Path::new(path);
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(WorkspaceRefusal::InvalidPath);
        }
        if !self.in_roots(path) {
            return Err(WorkspaceRefusal::OutsideRoots);
        }
        let resolved = resolve_missing(path, 0)?;
        if !self.in_roots(&resolved) {
            return Err(WorkspaceRefusal::OutsideRoots);
        }
        if overlaps(&resolved, &self.excluded) {
            return Err(WorkspaceRefusal::ProductHome);
        }
        let metadata = std::fs::metadata(&resolved).map_err(io_refusal)?;
        if !metadata.is_dir() {
            return Err(WorkspaceRefusal::NotDirectory);
        }
        let resolved = utf8(&canonical(&resolved)?)?;
        if require_canonical && path.as_os_str() != std::ffi::OsStr::new(&resolved) {
            return Err(WorkspaceRefusal::NotCanonical {
                canonical: resolved,
            });
        }
        Ok(resolved)
    }

    fn in_roots(&self, path: &Path) -> bool {
        self.roots
            .as_ref()
            .is_none_or(|roots| roots.iter().any(|root| path.starts_with(root)))
    }

    fn metadata_path(&self, path: &Path) -> Option<PathBuf> {
        let resolved = canonical(path).ok()?;
        (self.bases.iter().any(|base| resolved.starts_with(base))
            && !overlaps(&resolved, &self.excluded))
        .then_some(resolved)
    }

    /// Depth two, 200 checkouts, bounded directory entries and a five-second
    /// read deadline. A cached snapshot lives five seconds. Selection is fresh.
    #[must_use]
    pub fn discover(&self) -> WorkspaceDiscovery {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, discovery)) = cache.as_ref()
            && at.elapsed() < Duration::from_secs(5)
        {
            return discovery.clone();
        }
        let start = Instant::now();
        let mut found = BTreeMap::new();
        let mut remaining = 20_000_usize;
        let mut truncated = false;
        let mut pending = self
            .bases
            .iter()
            .map(|base| (base.clone(), 0))
            .collect::<Vec<_>>();
        while let Some((path, depth)) = pending.pop() {
            if remaining == 0 || start.elapsed() >= Duration::from_secs(5) {
                truncated = true;
                break;
            }
            remaining -= 1;
            let Some(path) = self.metadata_path(&path) else {
                continue;
            };
            let Ok(text) = utf8(&path) else {
                continue;
            };
            if path.join(".git").symlink_metadata().is_ok() {
                found.insert(
                    text.clone(),
                    DiscoveredWorkspace {
                        path: text,
                        repo: Some(self.repository(&path)),
                    },
                );
                continue;
            }
            if depth == 2 {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in entries {
                if remaining == 0 || start.elapsed() >= Duration::from_secs(5) {
                    truncated = true;
                    break;
                }
                remaining -= 1;
                let Ok(entry) = entry else {
                    continue;
                };
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    pending.push((entry.path(), depth + 1));
                }
            }
        }
        truncated |= found.len() >= 200;
        let discovery = WorkspaceDiscovery {
            workspaces: found.into_values().take(200).collect(),
            truncated,
        };
        *cache = Some((Instant::now(), discovery.clone()));
        discovery
    }

    fn repository(&self, path: &Path) -> RepositoryMetadata {
        let mut repo = RepositoryMetadata {
            name: path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            head: RepositoryHead::Unavailable,
            commit: None,
        };
        let Some(git) = self.metadata_path(&path.join(".git")) else {
            return repo;
        };
        let git = if git.is_file() {
            let Some(text) = self.read_metadata(&git) else {
                return repo;
            };
            let Some(target) = text.trim().strip_prefix("gitdir: ") else {
                return repo;
            };
            path.join(target)
        } else {
            git
        };
        let Some(git) = self.metadata_path(&git) else {
            return repo;
        };
        let Some(head) = self.read_metadata(&git.join("HEAD")) else {
            return repo;
        };
        let head = head.trim();
        if let Some(reference) = head.strip_prefix("ref: refs/heads/") {
            if !valid_ref(reference) {
                return repo;
            }
            let full = format!("refs/heads/{reference}");
            let common = if git.join("commondir").symlink_metadata().is_ok() {
                let Some(common) = self.read_metadata(&git.join("commondir")) else {
                    return repo;
                };
                let Some(common) = self.metadata_path(&git.join(common.trim())) else {
                    return repo;
                };
                common
            } else {
                git.clone()
            };
            let loose = common.join(&full);
            let commit = if loose.symlink_metadata().is_ok() {
                let Some(commit) = self
                    .read_metadata(&loose)
                    .and_then(|value| sha(value.trim()))
                else {
                    return repo;
                };
                Some(commit)
            } else {
                None
            };
            let commit = commit.or_else(|| {
                self.read_metadata(&common.join("packed-refs"))
                    .and_then(|text| {
                        text.lines().find_map(|line| {
                            let (hash, name) = line.split_once(' ')?;
                            (name == full).then(|| sha(hash)).flatten()
                        })
                    })
            });
            repo.head = if commit.is_some() {
                RepositoryHead::Branch {
                    name: reference.to_owned(),
                }
            } else {
                RepositoryHead::Unborn {
                    name: reference.to_owned(),
                }
            };
            repo.commit = commit;
        } else if let Some(commit) = sha(head) {
            repo.head = RepositoryHead::Detached;
            repo.commit = Some(commit);
        }
        repo
    }

    fn read_metadata(&self, path: &Path) -> Option<String> {
        let path = self.metadata_path(path)?;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(65_537)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > 65_536 {
            return None;
        }
        String::from_utf8(bytes).ok()
    }
}

fn valid_ref(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..")
        && !name.contains(['\\', ':', '~', '^', '?', '*', '['])
        && !name.chars().any(char::is_control)
        && Path::new(name)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn sha(text: &str) -> Option<String> {
    ((text.len() == 40 || text.len() == 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| text.to_ascii_lowercase())
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}
fn utf8(path: &Path) -> Result<String, WorkspaceRefusal> {
    path.to_str()
        .map(str::to_owned)
        .ok_or(WorkspaceRefusal::InvalidPath)
}
fn canonical(path: &Path) -> Result<PathBuf, WorkspaceRefusal> {
    path.canonicalize().map_err(io_refusal)
}
fn io_refusal(error: io::Error) -> WorkspaceRefusal {
    match error.kind() {
        io::ErrorKind::NotFound => WorkspaceRefusal::NotFound,
        io::ErrorKind::NotADirectory => WorkspaceRefusal::NotDirectory,
        _ => WorkspaceRefusal::PermissionDenied,
    }
}

fn resolve_missing(path: &Path, depth: usize) -> Result<PathBuf, WorkspaceRefusal> {
    if depth > 40 {
        return Err(WorkspaceRefusal::InvalidPath);
    }
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            _ => resolved.push(component.as_os_str()),
        }
        match std::fs::symlink_metadata(&resolved) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = std::fs::read_link(&resolved).map_err(io_refusal)?;
                let target = if target.is_absolute() {
                    target
                } else {
                    resolved.parent().unwrap_or(Path::new("/")).join(target)
                };
                resolved = resolve_missing(&target, depth + 1)?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_refusal(error)),
        }
    }
    Ok(resolved)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ValidationRequest {
    path: String,
    require_canonical: bool,
}

pub(super) fn serve(
    catalog: Option<std::sync::Arc<WorkspaceCatalog>>,
    verb: plakous_tools::internal::runner::RunnerVerb,
    payload: serde_json::Value,
) -> plakous_tools::internal::runner::ServeFuture {
    use plakous_tools::internal::runner::{RunnerVerb, ServeFailure};
    let Some(catalog) = catalog else {
        return Box::pin(std::future::ready(Err(ServeFailure::Unsupported)));
    };
    Box::pin(async move {
        tokio::task::spawn_blocking(move || {
            let result = match verb {
                RunnerVerb::WorkspaceOptions if payload == serde_json::json!({}) => {
                    serde_json::to_value(catalog.options())
                }
                RunnerVerb::WorkspaceDiscover if payload == serde_json::json!({}) => {
                    serde_json::to_value(catalog.discover())
                }
                RunnerVerb::WorkspaceValidate => {
                    let request: ValidationRequest =
                        serde_json::from_value(payload).map_err(|_| ServeFailure::Unsupported)?;
                    serde_json::to_value(catalog.validate(&request.path, request.require_canonical))
                }
                _ => return Err(ServeFailure::Unsupported),
            };
            result.map_err(|_| ServeFailure::Unsupported)
        })
        .await
        .unwrap_or(Err(ServeFailure::Unsupported))
    })
}

impl crate::RunnerClient {
    /// Effective roots from this client's existing paired connection. No second bind.
    ///
    /// # Errors
    /// Returns transport/protocol failure, never an empty synthetic catalog.
    pub async fn workspace_options(
        &self,
    ) -> Result<WorkspaceOptions, plakous_tools::internal::runner::RunnerFailure> {
        self.workspace_read(
            plakous_tools::internal::runner::RunnerVerb::WorkspaceOptions,
            serde_json::json!({}),
        )
        .await
    }

    /// Cached bounded checkout discovery on the selected runner.
    ///
    /// # Errors
    /// Returns transport/protocol failure independently of other runners.
    pub async fn workspace_discover(
        &self,
    ) -> Result<WorkspaceDiscovery, plakous_tools::internal::runner::RunnerFailure> {
        self.workspace_read(
            plakous_tools::internal::runner::RunnerVerb::WorkspaceDiscover,
            serde_json::json!({}),
        )
        .await
    }

    /// Validates on the runner, never on the host. The inner error is an
    /// admission refusal; the outer error is a link/protocol failure.
    ///
    /// # Errors
    /// Returns transport/protocol failure without admitting any effect.
    pub async fn workspace_validate(
        &self,
        path: &str,
        require_canonical: bool,
    ) -> Result<Result<String, WorkspaceRefusal>, plakous_tools::internal::runner::RunnerFailure>
    {
        self.workspace_read(
            plakous_tools::internal::runner::RunnerVerb::WorkspaceValidate,
            serde_json::json!({ "path": path, "require_canonical": require_canonical }),
        )
        .await
    }

    async fn workspace_read<T: serde::de::DeserializeOwned>(
        &self,
        verb: plakous_tools::internal::runner::RunnerVerb,
        payload: serde_json::Value,
    ) -> Result<T, plakous_tools::internal::runner::RunnerFailure> {
        use plakous_tools::internal::runner::{RunnerCall, RunnerFailure, RunnerLink};
        let value = self.call(RunnerCall::unkeyed(verb, payload)).await?;
        serde_json::from_value(value)
            .map_err(|_| RunnerFailure::Unavailable("invalid workspace response".into()))
    }
}
