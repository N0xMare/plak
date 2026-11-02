//! Bounded Plak-owned discovery of model-visible instruction files.
//!
//! Discovery reads only through an [`InstructionSourceReader`]; every bound,
//! scope and digest is decided here. The returned immutable bytes and
//! identities are suitable for enrollment through the public Plakous
//! context-source facade.

use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use plakous_agent::{ProfileError, RegisteredContextSource};
use plakous_model::{hex_lower, sha256_bytes};

mod reader;
pub use reader::{
    DirectoryAnswer, DirectoryRefusal, InstructionChainEntry, InstructionSourceReader,
    LocalInstructionSourceReader, SourceEntryKind, SourceFile, SourceReadError, SourceResolution,
};

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
/// reads: `global`, `workspace root`, `skills listing`, or `nested directory`.
/// Nested ids carry only a digest of their directory, so the directory itself
/// is not named.
#[must_use]
pub fn instruction_source_scope_label(source_id: &str) -> &'static str {
    match source_id {
        GLOBAL_SOURCE_ID => "global",
        WORKSPACE_ROOT_SOURCE_ID => "workspace root",
        crate::skills::SKILLS_SOURCE_ID => "skills listing",
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
    /// The reader could not answer, or answered outside its contract.
    Reader(SourceReadError),
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
            Self::Reader(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for InstructionDiscoveryError {}

/// Discovers the optional product-global file followed by the workspace
/// root-to-cwd `AGENTS.md` chain through `reader`. All returned paths are
/// physical absolute paths and all reads are anchored to no-follow directory
/// descriptors. Every bound, the aggregate budget, UTF-8, ids and labels are
/// decided here from the reader's raw answers; a reader that cannot answer,
/// or answers outside its contract, fails discovery.
///
/// # Errors
///
/// Returns the bound that refused: an out-of-workspace cwd, a walk deeper than
/// [`MAX_WORKSPACE_DEPTH`], an over-long or unsafe path, a non-regular file,
/// content over [`MAX_DISCOVERED_CONTENT_BYTES`], more than
/// [`MAX_DISCOVERED_SOURCES`] files, invalid UTF-8, an I/O failure, or the
/// reader's [`SourceReadError`].
pub fn discover_instruction_sources(
    reader: &dyn InstructionSourceReader,
    product_home: Option<&Path>,
    workspace_root: &Path,
    cwd: &Path,
) -> Result<InstructionDiscovery, InstructionDiscoveryError> {
    let root = canonical_directory(reader, workspace_root)?;
    let cwd = canonical_directory(reader, cwd)?;
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
        match reader
            .canonicalize(home)
            .map_err(InstructionDiscoveryError::Reader)?
        {
            SourceResolution::Resolved(physical_home) => {
                check_path_bound(&physical_home)?;
                let global = read_chain(reader, &physical_home, &[], &mut remaining)?;
                if let Some(content) = global.into_iter().flatten().next() {
                    sources.push(source(
                        GLOBAL_SOURCE_ID.to_owned(),
                        InstructionScope::Global,
                        content,
                    ));
                }
            }
            SourceResolution::NotFound => {}
            SourceResolution::Unresolvable => {
                return Err(InstructionDiscoveryError::UnsafePath { path: home.into() });
            }
        }
    }

    let chain = read_chain(reader, &root, &components, &mut remaining)?;
    let mut relative_directory = PathBuf::new();
    for (index, content) in chain.into_iter().enumerate() {
        if let Some(content) = content {
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

fn canonical_directory(
    reader: &dyn InstructionSourceReader,
    path: &Path,
) -> Result<PathBuf, InstructionDiscoveryError> {
    let SourceResolution::Resolved(canonical) = reader
        .canonicalize(path)
        .map_err(InstructionDiscoveryError::Reader)?
    else {
        return Err(InstructionDiscoveryError::Io { path: path.into() });
    };
    check_path_bound(&canonical)?;
    match reader
        .anchored_directory(&canonical)
        .map_err(InstructionDiscoveryError::Reader)?
    {
        DirectoryAnswer::Directory => Ok(canonical),
        DirectoryAnswer::NotDirectory => {
            Err(InstructionDiscoveryError::UnsafePath { path: path.into() })
        }
        DirectoryAnswer::Refused(refusal) => Err(refused_directory(refusal, &canonical)),
    }
}

/// Reads one chain through `reader` and validates every answer in walk order:
/// the chain's shape, each file path's bound, the per-read limit, the
/// aggregate budget `remaining` (spent only by accepted content) and UTF-8.
fn read_chain(
    reader: &dyn InstructionSourceReader,
    start: &Path,
    components: &[OsString],
    remaining: &mut usize,
) -> Result<Vec<Option<String>>, InstructionDiscoveryError> {
    let limit = *remaining;
    let entries = reader
        .read_instruction_chain(start, components, limit)
        .map_err(InstructionDiscoveryError::Reader)?;
    let complete = entries.len() == components.len() + 1
        && entries
            .iter()
            .all(|entry| matches!(entry, InstructionChainEntry::File(_)));
    let stopped = entries.len() <= components.len() + 1
        && entries.split_last().is_some_and(|(last, before)| {
            matches!(last, InstructionChainEntry::Unopened(_))
                && before
                    .iter()
                    .all(|entry| matches!(entry, InstructionChainEntry::File(_)))
        });
    if !complete && !stopped {
        return Err(InstructionDiscoveryError::Reader(
            SourceReadError::InvalidAnswer,
        ));
    }
    let mut contents = Vec::with_capacity(entries.len());
    let mut directory = start.to_path_buf();
    for (index, entry) in entries.into_iter().enumerate() {
        let file = match entry {
            InstructionChainEntry::File(file) => file,
            InstructionChainEntry::Unopened(refusal) => {
                return Err(refused_directory(refusal, &directory));
            }
        };
        let path = directory.join(INSTRUCTION_FILE_NAME);
        check_path_bound(&path)?;
        contents.push(validated_content(file, path, limit, remaining)?);
        if let Some(component) = components.get(index) {
            directory.push(component);
        }
    }
    Ok(contents)
}

fn validated_content(
    file: SourceFile,
    path: PathBuf,
    limit: usize,
    remaining: &mut usize,
) -> Result<Option<String>, InstructionDiscoveryError> {
    let too_large = || InstructionDiscoveryError::ContentTooLarge {
        max_bytes: MAX_DISCOVERED_CONTENT_BYTES,
    };
    let bytes = match file {
        SourceFile::Missing => return Ok(None),
        SourceFile::Unsafe => return Err(InstructionDiscoveryError::UnsafePath { path }),
        SourceFile::NotRegular => return Err(InstructionDiscoveryError::NotRegularFile { path }),
        SourceFile::Unreadable => return Err(InstructionDiscoveryError::Io { path }),
        SourceFile::TooLarge => return Err(too_large()),
        SourceFile::Bytes(bytes) => bytes,
    };
    if bytes.len() > limit.saturating_add(1) {
        return Err(InstructionDiscoveryError::Reader(
            SourceReadError::InvalidAnswer,
        ));
    }
    if bytes.len() > *remaining {
        return Err(too_large());
    }
    let content =
        String::from_utf8(bytes).map_err(|_| InstructionDiscoveryError::InvalidUtf8 { path })?;
    *remaining -= content.len();
    Ok(Some(content))
}

fn refused_directory(refusal: DirectoryRefusal, directory: &Path) -> InstructionDiscoveryError {
    match refusal {
        DirectoryRefusal::Unsafe { path } => InstructionDiscoveryError::UnsafePath { path },
        DirectoryRefusal::Unreadable => InstructionDiscoveryError::Io {
            path: directory.into(),
        },
    }
}

fn normal_components(path: &Path) -> Result<Vec<OsString>, InstructionDiscoveryError> {
    path.components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value.to_os_string()),
            Component::CurDir => Ok(OsString::new()),
            _ => Err(InstructionDiscoveryError::UnsafePath { path: path.into() }),
        })
        .filter(|value| value.as_ref().is_ok_and(|part| !part.is_empty()) || value.is_err())
        .collect()
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
mod tests;
