//! Bounded Plak-owned discovery of model-visible instruction files.
//!
//! Filesystem access ends here. The returned immutable bytes and identities are
//! suitable for enrollment through the public Plakous context-source facade.

use std::fmt;
use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};

use plakous_agent::{ProfileError, RegisteredContextSource};
use plakous_model::{hex_lower, sha256_bytes};
use rustix::fs::{Mode, OFlags};

/// The one instruction file name discovery looks for.
pub const INSTRUCTION_FILE_NAME: &str = "AGENTS.md";
/// Maximum directories walked from the workspace root down to the cwd.
pub const MAX_WORKSPACE_DEPTH: usize = 30;
/// Maximum instruction files enrolled from one discovery.
pub const MAX_DISCOVERED_SOURCES: usize = 32;
/// Maximum bytes retained from one instruction file.
pub const MAX_DISCOVERED_CONTENT_BYTES: usize = 60 * 1024;
/// Maximum bytes in one instruction file path.
pub const MAX_INSTRUCTION_PATH_BYTES: usize = 4 * 1024;

const CONTENT_DIGEST_DOMAIN: &[u8] = b"plak.instruction-source.content.v1";

const GLOBAL_SOURCE_ID: &str = "plak.agents.global.v1";
const WORKSPACE_ROOT_SOURCE_ID: &str = "plak.agents.workspace-root.v1";

/// Where an enrolled instruction source id was discovered, in words a person
/// reads: `global`, `workspace root`, or `nested directory`. Nested ids carry
/// only a digest of their directory, so the directory itself is not named.
#[must_use]
pub fn instruction_source_scope_label(source_id: &str) -> &'static str {
    match source_id {
        GLOBAL_SOURCE_ID => "global",
        WORKSPACE_ROOT_SOURCE_ID => "workspace root",
        _ => "nested directory",
    }
}

/// The scope label carries the digest prefix, not the whole digest.
const SCOPE_LABEL_DIGEST_BYTES: usize = 8;

/// Where one discovered instruction file came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstructionScope {
    /// The operator's home instruction file, outside any workspace.
    Global,
    /// One directory of the workspace, relative to its root.
    Workspace {
        /// The directory this file was found in, relative to the workspace root.
        relative_directory: PathBuf,
    },
}

impl InstructionScope {
    fn scope_label(&self) -> String {
        match self {
            Self::Global => "global".to_owned(),
            Self::Workspace { relative_directory } if relative_directory.as_os_str().is_empty() => {
                "workspace-root".to_owned()
            }
            Self::Workspace { relative_directory } => {
                let digest = sha256_bytes(&[
                    b"plak.instruction-source.scope.v1",
                    &[0],
                    relative_directory.as_os_str().as_encoded_bytes(),
                ]);
                format!(
                    "workspace-{}",
                    hex_lower(&digest[..SCOPE_LABEL_DIGEST_BYTES])
                )
            }
        }
    }

    fn provenance(&self) -> String {
        format!("plak-agents-v1:{}", self.scope_label())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// One discovered instruction file: its stable id, where it came from, its
/// bounded content, and the digest that labels the version observed.
#[allow(
    clippy::module_name_repetitions,
    reason = "the type names the file kind, not the module"
)]
pub struct DiscoveredInstructionSource {
    source_id: String,
    scope: InstructionScope,
    content: String,
    content_digest: String,
}

impl DiscoveredInstructionSource {
    /// Returns the stable enrollment id.
    #[must_use]
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// Returns where this file was found.
    #[cfg(test)]
    #[must_use]
    pub const fn scope(&self) -> &InstructionScope {
        &self.scope
    }

    /// Returns the bounded file content.
    #[cfg(test)]
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Returns the observed-version label.
    #[cfg(test)]
    #[must_use]
    pub fn content_digest(&self) -> &str {
        &self.content_digest
    }

    /// Materializes the pure facade input.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the source cannot be registered. The digest is the observed-version
    /// label, while Plakous independently binds its own exact-content digest.
    pub fn registered_source(&self) -> Result<RegisteredContextSource, ProfileError> {
        RegisteredContextSource::new(
            self.source_id.clone(),
            self.content.clone(),
            self.content_digest.clone(),
            self.scope.provenance(),
        )
    }
}

/// The complete result of one bounded discovery, in enrollment order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionDiscovery {
    sources: Vec<DiscoveredInstructionSource>,
}

impl InstructionDiscovery {
    /// Returns the discovered files, global scope first then root to cwd.
    #[must_use]
    pub fn sources(&self) -> &[DiscoveredInstructionSource] {
        &self.sources
    }
}

/// Bytes of enrolled instruction sources as they were inlined into composed
/// instructions: trailing `\n\n{content}` (or a bare suffix) stripped from
/// the last-compiled text. Occupancy uses this rather than rediscovering
/// files at paint, so a drifted `AGENTS.md` cannot change the split.
#[must_use]
pub fn inlined_instruction_source_bytes(composed: &str, contents: &[&str]) -> u64 {
    let mut remaining = composed.to_owned();
    let mut removed = 0usize;
    for content in contents.iter().rev() {
        if content.is_empty() {
            continue;
        }
        let separated = format!("\n\n{content}");
        if remaining.ends_with(&separated) {
            remaining.truncate(remaining.len() - separated.len());
            removed += separated.len();
        } else if remaining.ends_with(*content) {
            remaining.truncate(remaining.len() - content.len());
            removed += content.len();
        }
    }
    u64::try_from(removed).unwrap_or(u64::MAX)
}

/// Every way a bounded discovery refuses. Each names the bound it hit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstructionDiscoveryError {
    /// The cwd is not inside the workspace root.
    InvalidWorkspaceBoundary {
        /// The recorded workspace root.
        root: PathBuf,
        /// The current directory that fell outside it.
        cwd: PathBuf,
    },
    /// The root-to-cwd walk is deeper than [`MAX_WORKSPACE_DEPTH`].
    WorkspaceDepthExceeded {
        /// The bound that was exceeded.
        max_edges: usize,
    },
    /// One candidate path is longer than [`MAX_INSTRUCTION_PATH_BYTES`].
    PathTooLong {
        /// The path that was refused.
        path: PathBuf,
        /// The bound that was exceeded.
        max_bytes: usize,
    },
    /// One candidate path is not a plain descent from the root.
    UnsafePath {
        /// The path that was refused.
        path: PathBuf,
    },
    /// One candidate path is a directory, symlink, or device.
    NotRegularFile {
        /// The path that was refused.
        path: PathBuf,
    },
    /// One file is larger than [`MAX_DISCOVERED_CONTENT_BYTES`].
    ContentTooLarge {
        /// The bound that was exceeded.
        max_bytes: usize,
    },
    /// More than [`MAX_DISCOVERED_SOURCES`] files were found.
    TooManySources {
        /// The bound that was exceeded.
        max_sources: usize,
    },
    /// One file is not valid UTF-8.
    InvalidUtf8 {
        /// The path that was refused.
        path: PathBuf,
    },
    /// One file could not be opened or read.
    Io {
        /// The path that was refused.
        path: PathBuf,
    },
}

impl fmt::Display for InstructionDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidWorkspaceBoundary { root, cwd } => write!(
                formatter,
                "instruction cwd {} is outside workspace root {}",
                cwd.display(),
                root.display()
            ),
            Self::WorkspaceDepthExceeded { max_edges } => write!(
                formatter,
                "instruction workspace depth exceeds {max_edges} edges"
            ),
            Self::PathTooLong { path, max_bytes } => write!(
                formatter,
                "instruction path {} exceeds {max_bytes} bytes",
                path.display()
            ),
            Self::UnsafePath { path } => {
                write!(formatter, "instruction path {} is unsafe", path.display())
            }
            Self::NotRegularFile { path } => write!(
                formatter,
                "instruction path {} is not a regular file",
                path.display()
            ),
            Self::ContentTooLarge { max_bytes } => write!(
                formatter,
                "discovered instruction content exceeds {max_bytes} bytes"
            ),
            Self::TooManySources { max_sources } => write!(
                formatter,
                "discovered instruction source count exceeds {max_sources}"
            ),
            Self::InvalidUtf8 { path } => write!(
                formatter,
                "instruction file {} is not valid UTF-8",
                path.display()
            ),
            Self::Io { path } => write!(
                formatter,
                "could not inspect instruction path {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for InstructionDiscoveryError {}

/// Discovers the optional product-global file followed by the workspace
/// root-to-cwd `AGENTS.md` chain. All returned paths are physical absolute
/// paths and all reads are anchored to no-follow directory descriptors.
///
/// # Errors
///
/// Returns the bound that refused: an out-of-workspace cwd, a walk deeper than
/// [`MAX_WORKSPACE_DEPTH`], an over-long or unsafe path, a non-regular file,
/// content over [`MAX_DISCOVERED_CONTENT_BYTES`], more than
/// [`MAX_DISCOVERED_SOURCES`] files, invalid UTF-8, or an I/O failure.
pub fn discover_instruction_sources(
    product_home: Option<&Path>,
    workspace_root: &Path,
    cwd: &Path,
) -> Result<InstructionDiscovery, InstructionDiscoveryError> {
    let root = canonical_directory(workspace_root)?;
    let cwd = canonical_directory(cwd)?;
    let relative = cwd.strip_prefix(&root).map_err(|_| {
        InstructionDiscoveryError::InvalidWorkspaceBoundary {
            root: root.clone(),
            cwd: cwd.clone(),
        }
    })?;
    let components = normal_components(relative)?;
    if components.len() > MAX_WORKSPACE_DEPTH {
        return Err(InstructionDiscoveryError::WorkspaceDepthExceeded {
            max_edges: MAX_WORKSPACE_DEPTH,
        });
    }

    let mut sources = Vec::new();
    let mut remaining = MAX_DISCOVERED_CONTENT_BYTES;

    if let Some(home) = product_home {
        match std::fs::canonicalize(home) {
            Ok(physical_home) => {
                check_path_bound(&physical_home)?;
                let home_directory = open_absolute_directory(&physical_home)?;
                if let Some(content) = read_optional_instruction(
                    &home_directory,
                    &physical_home.join(INSTRUCTION_FILE_NAME),
                    remaining,
                )? {
                    remaining -= content.len();
                    sources.push(source(
                        GLOBAL_SOURCE_ID.to_owned(),
                        InstructionScope::Global,
                        content,
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(InstructionDiscoveryError::UnsafePath { path: home.into() }),
        }
    }

    let mut directory = open_absolute_directory(&root)?;
    let mut physical_directory = root.clone();
    let mut relative_directory = PathBuf::new();
    for index in 0..=components.len() {
        let path = physical_directory.join(INSTRUCTION_FILE_NAME);
        if let Some(content) = read_optional_instruction(&directory, &path, remaining)? {
            remaining -= content.len();
            let scope = InstructionScope::Workspace {
                relative_directory: relative_directory.clone(),
            };
            sources.push(source(
                format!("plak.agents.{}.v1", scope.scope_label()),
                scope,
                content,
            ));
        }
        if let Some(component) = components.get(index) {
            directory = open_child_directory(&directory, component, &physical_directory)?;
            physical_directory.push(component);
            relative_directory.push(component);
        }
    }
    if sources.len() > MAX_DISCOVERED_SOURCES {
        return Err(InstructionDiscoveryError::TooManySources {
            max_sources: MAX_DISCOVERED_SOURCES,
        });
    }
    Ok(InstructionDiscovery { sources })
}

fn canonical_directory(path: &Path) -> Result<PathBuf, InstructionDiscoveryError> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| InstructionDiscoveryError::Io { path: path.into() })?;
    check_path_bound(&canonical)?;
    let directory = open_absolute_directory(&canonical)?;
    directory
        .metadata()
        .map_err(|_| InstructionDiscoveryError::Io {
            path: canonical.clone(),
        })?
        .is_dir()
        .then_some(canonical)
        .ok_or_else(|| InstructionDiscoveryError::UnsafePath { path: path.into() })
}

fn normal_components(path: &Path) -> Result<Vec<std::ffi::OsString>, InstructionDiscoveryError> {
    path.components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value.to_os_string()),
            Component::CurDir => Ok(std::ffi::OsString::new()),
            _ => Err(InstructionDiscoveryError::UnsafePath { path: path.into() }),
        })
        .filter(|value| value.as_ref().is_ok_and(|part| !part.is_empty()) || value.is_err())
        .collect()
}

fn open_absolute_directory(path: &Path) -> Result<File, InstructionDiscoveryError> {
    if !path.is_absolute() {
        return Err(InstructionDiscoveryError::UnsafePath { path: path.into() });
    }
    let mut directory =
        File::open("/").map_err(|_| InstructionDiscoveryError::Io { path: path.into() })?;
    let mut traversed = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => {
                directory = open_child_directory(&directory, value, &traversed)?;
                traversed.push(value);
            }
            _ => return Err(InstructionDiscoveryError::UnsafePath { path: path.into() }),
        }
    }
    Ok(directory)
}

fn open_child_directory(
    parent: &File,
    name: &std::ffi::OsStr,
    parent_path: &Path,
) -> Result<File, InstructionDiscoveryError> {
    rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| InstructionDiscoveryError::UnsafePath {
        path: parent_path.join(name),
    })
}

fn read_optional_instruction(
    directory: &File,
    path: &Path,
    remaining: usize,
) -> Result<Option<String>, InstructionDiscoveryError> {
    check_path_bound(path)?;
    let descriptor = match rustix::fs::openat(
        directory,
        INSTRUCTION_FILE_NAME,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(_) => return Err(InstructionDiscoveryError::UnsafePath { path: path.into() }),
    };
    let mut file = File::from(descriptor);
    if !file
        .metadata()
        .map_err(|_| InstructionDiscoveryError::Io { path: path.into() })?
        .is_file()
    {
        return Err(InstructionDiscoveryError::NotRegularFile { path: path.into() });
    }
    let limit = remaining
        .checked_add(1)
        .ok_or(InstructionDiscoveryError::ContentTooLarge {
            max_bytes: MAX_DISCOVERED_CONTENT_BYTES,
        })?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| InstructionDiscoveryError::Io { path: path.into() })?;
    if bytes.len() > remaining {
        return Err(InstructionDiscoveryError::ContentTooLarge {
            max_bytes: MAX_DISCOVERED_CONTENT_BYTES,
        });
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| InstructionDiscoveryError::InvalidUtf8 { path: path.into() })
}

fn check_path_bound(path: &Path) -> Result<(), InstructionDiscoveryError> {
    if path.as_os_str().as_encoded_bytes().len() > MAX_INSTRUCTION_PATH_BYTES {
        return Err(InstructionDiscoveryError::PathTooLong {
            path: path.into(),
            max_bytes: MAX_INSTRUCTION_PATH_BYTES,
        });
    }
    Ok(())
}

fn source(
    source_id: String,
    scope: InstructionScope,
    content: String,
) -> DiscoveredInstructionSource {
    let content_length = (content.len() as u64).to_be_bytes();
    let digest = sha256_bytes(&[
        CONTENT_DIGEST_DOMAIN,
        &[0],
        &content_length,
        content.as_bytes(),
    ]);
    let content_digest = format!("sha256:{}", hex_lower(&digest));
    DiscoveredInstructionSource {
        source_id,
        scope,
        content,
        content_digest,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn discovers_global_then_root_to_cwd_with_exact_enrollment_inputs() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let root = temporary.path().join("repo");
        let nested = root.join("a/b");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::write(home.join(INSTRUCTION_FILE_NAME), "global").unwrap();
        fs::write(root.join(INSTRUCTION_FILE_NAME), "root").unwrap();
        fs::write(root.join("a").join(INSTRUCTION_FILE_NAME), "a").unwrap();
        fs::write(nested.join(INSTRUCTION_FILE_NAME), "b").unwrap();

        let discovered = discover_instruction_sources(Some(&home), &root, &nested).unwrap();
        let sources = discovered.sources();
        assert_eq!(
            sources
                .iter()
                .map(DiscoveredInstructionSource::content)
                .collect::<Vec<_>>(),
            ["global", "root", "a", "b"]
        );
        assert_eq!(sources[0].source_id(), "plak.agents.global.v1");
        assert_eq!(
            instruction_source_scope_label(sources[0].source_id()),
            "global"
        );
        assert_eq!(
            instruction_source_scope_label(sources[1].source_id()),
            "workspace root"
        );
        assert_eq!(
            instruction_source_scope_label(sources[3].source_id()),
            "nested directory"
        );
        assert!(sources[3].source_id().starts_with("plak.agents.workspace-"));
        assert_eq!(
            sources[2].scope(),
            &InstructionScope::Workspace {
                relative_directory: PathBuf::from("a")
            }
        );
        assert!(sources[0].content_digest().starts_with("sha256:"));
        let registered = sources[3].registered_source().unwrap();
        assert_eq!(registered.source_id(), sources[3].source_id());
        assert_eq!(registered.content(), sources[3].content());
        assert_eq!(registered.version(), sources[3].content_digest());
        assert_eq!(
            registered.provenance(),
            format!("plak-agents-v1:{}", sources[3].scope().scope_label())
        );
    }

    #[test]
    fn missing_optional_sources_are_not_errors() {
        let temporary = tempfile::tempdir().unwrap();
        let missing_home = temporary.path().join("missing-home");
        let discovered =
            discover_instruction_sources(Some(&missing_home), temporary.path(), temporary.path())
                .unwrap();
        assert_eq!(discovered.sources(), []);
    }

    #[test]
    fn inlined_source_bytes_strip_the_composed_suffix_not_a_file_length() {
        let content = "repo agents";
        let composed = format!("core prompt\n\n{content}");
        assert_eq!(
            inlined_instruction_source_bytes(&composed, &[content]),
            "\n\nrepo agents".len() as u64
        );
        assert_eq!(
            inlined_instruction_source_bytes("core prompt only", &[content]),
            0,
            "facts that are not inlined must not split occupancy"
        );
        assert_eq!(
            inlined_instruction_source_bytes(content, &[content]),
            content.len() as u64
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_instruction_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target");
        fs::write(&target, "outside").unwrap();
        symlink(&target, temporary.path().join(INSTRUCTION_FILE_NAME)).unwrap();

        let error =
            discover_instruction_sources(None, temporary.path(), temporary.path()).unwrap_err();
        assert!(matches!(
            error,
            InstructionDiscoveryError::UnsafePath { .. }
        ));
    }

    #[test]
    fn rejects_cwd_outside_workspace_and_excessive_depth() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        assert!(matches!(
            discover_instruction_sources(None, &root, &outside),
            Err(InstructionDiscoveryError::InvalidWorkspaceBoundary { .. })
        ));

        let mut deep = root.clone();
        for index in 0..=MAX_WORKSPACE_DEPTH {
            deep.push(format!("d{index}"));
        }
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(
            discover_instruction_sources(None, &root, &deep).unwrap_err(),
            InstructionDiscoveryError::WorkspaceDepthExceeded {
                max_edges: MAX_WORKSPACE_DEPTH
            }
        );
    }

    #[test]
    fn enforces_aggregate_byte_bound_and_utf8() {
        let oversized = tempfile::tempdir().unwrap();
        fs::write(
            oversized.path().join(INSTRUCTION_FILE_NAME),
            vec![b'x'; MAX_DISCOVERED_CONTENT_BYTES + 1],
        )
        .unwrap();
        assert_eq!(
            discover_instruction_sources(None, oversized.path(), oversized.path()).unwrap_err(),
            InstructionDiscoveryError::ContentTooLarge {
                max_bytes: MAX_DISCOVERED_CONTENT_BYTES
            }
        );

        let invalid = tempfile::tempdir().unwrap();
        fs::write(invalid.path().join(INSTRUCTION_FILE_NAME), [0xff]).unwrap();
        assert!(matches!(
            discover_instruction_sources(None, invalid.path(), invalid.path()),
            Err(InstructionDiscoveryError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn content_digest_is_domain_separated_and_deterministic() {
        let first = source(
            "one".to_owned(),
            InstructionScope::Global,
            "same".to_owned(),
        );
        let second = source(
            "two".to_owned(),
            InstructionScope::Global,
            "same".to_owned(),
        );
        assert_eq!(first.content_digest(), second.content_digest());
        assert_eq!(first.content_digest().len(), 71);
    }

    #[test]
    fn workspace_scope_ids_distinguish_same_depth_directories() {
        let left = InstructionScope::Workspace {
            relative_directory: PathBuf::from("left/child"),
        };
        let right = InstructionScope::Workspace {
            relative_directory: PathBuf::from("right/child"),
        };
        assert_ne!(left.scope_label(), right.scope_label());
        assert_ne!(left.provenance(), right.provenance());
    }
}
