//! Where instruction and skill discovery reads files.
//!
//! A reader answers raw, bounded questions about single paths and nothing
//! else; discovery keeps every bound, budget, scope, label and validation and
//! checks each answer against the request it made, so a reader that answers
//! outside its contract is refused rather than trusted.
//!
//! The port is synchronous in this step. It becomes async in a later step,
//! together with the version-control port, once session construction is
//! async; until then a caller must not hold a lock across a call.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Mode, OFlags};

use super::INSTRUCTION_FILE_NAME;

/// Why a reader produced no answer at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SourceReadError {
    /// The reader could not answer: the host the files live on is
    /// unreachable. Distinct from every "absent" answer, so discovery fails
    /// closed instead of freezing an empty result.
    Unavailable,
    /// The reader answered outside its contract: more bytes than the read
    /// admits, a chain of the wrong length, an entry that is not one name.
    /// Discovery raises this; a reader never needs to.
    InvalidAnswer,
}

impl fmt::Display for SourceReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "the instruction source reader is unavailable",
            Self::InvalidAnswer => "the instruction source reader answered outside its contract",
        })
    }
}

impl std::error::Error for SourceReadError {}

/// How one path resolved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceResolution {
    /// The physical absolute path.
    Resolved(PathBuf),
    /// Nothing exists at the path.
    NotFound,
    /// The path exists but could not be resolved.
    Unresolvable,
}

/// What one path names, without following a final link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceEntryKind {
    /// A symbolic link.
    Link,
    /// A directory.
    Directory,
    /// A regular file, socket, device or FIFO.
    Other,
}

/// One file read: the raw answer for a single path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceFile {
    /// Nothing exists at the path.
    Missing,
    /// The path names a directory, device, socket or FIFO.
    NotRegular,
    /// The path is a link, or its open was refused.
    Unsafe,
    /// The file opened but could not be inspected or read.
    Unreadable,
    /// The file is longer than the read admits.
    TooLarge,
    /// At most `limit + 1` bytes of the file, so the caller can tell a file
    /// longer than `limit`.
    Bytes(Vec<u8>),
}

/// Why a directory could not be opened from `/` without following a link.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirectoryRefusal {
    /// A component is a link, or its open was refused; `path` names it.
    Unsafe {
        /// The component that could not be opened.
        path: PathBuf,
    },
    /// The walk could not start, or the opened directory could not be
    /// inspected.
    Unreadable,
}

/// What a physical absolute path names when opened from `/` without
/// following any link.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirectoryAnswer {
    /// A directory.
    Directory,
    /// Something that is not a directory.
    NotDirectory,
    /// The open was refused.
    Refused(DirectoryRefusal),
}

/// One directory of an instruction chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstructionChainEntry {
    /// The directory opened; the answer for its instruction file.
    File(SourceFile),
    /// The directory could not be opened; the walk stopped here, so this is
    /// the chain's last entry.
    Unopened(DirectoryRefusal),
}

/// Reads instruction and skill files where they live.
///
/// [`discover_instruction_sources`](super::discover_instruction_sources) and
/// the Agent Skills discovery and load in [`crate::skills`] reach files only
/// through this port. Every answer is fallible with
/// [`SourceReadError::Unavailable`], which is never an "absent" answer.
/// [`LocalInstructionSourceReader`] reads this process's filesystem.
pub trait InstructionSourceReader: Send + Sync {
    /// How `path` resolves, as [`std::fs::canonicalize`] would.
    ///
    /// # Errors
    /// Returns [`SourceReadError::Unavailable`] when the reader cannot answer.
    fn canonicalize(&self, path: &Path) -> Result<SourceResolution, SourceReadError>;

    /// What the physical absolute `directory` names, opened from `/` without
    /// following any link.
    ///
    /// # Errors
    /// Returns [`SourceReadError::Unavailable`] when the reader cannot answer.
    fn anchored_directory(&self, directory: &Path) -> Result<DirectoryAnswer, SourceReadError>;

    /// One entry per directory, starting at `directory` and descending along
    /// `components`: the answer for [`INSTRUCTION_FILE_NAME`] in each, read
    /// without following a link or blocking, at most `limit + 1` bytes each.
    /// Every directory is opened from `/` without following a link; a
    /// directory that cannot be opened ends the chain with
    /// [`InstructionChainEntry::Unopened`]. A complete chain has
    /// `components.len() + 1` entries.
    ///
    /// # Errors
    /// Returns [`SourceReadError::Unavailable`] when the reader cannot answer.
    fn read_instruction_chain(
        &self,
        directory: &Path,
        components: &[OsString],
        limit: usize,
    ) -> Result<Vec<InstructionChainEntry>, SourceReadError>;

    /// Whether `path` names a directory, following links.
    ///
    /// # Errors
    /// Returns [`SourceReadError::Unavailable`] when the reader cannot answer.
    fn is_directory(&self, path: &Path) -> Result<bool, SourceReadError>;

    /// What `path` names, without following a final link; `None` when it
    /// cannot be inspected.
    ///
    /// # Errors
    /// Returns [`SourceReadError::Unavailable`] when the reader cannot answer.
    fn entry_kind(&self, path: &Path) -> Result<Option<SourceEntryKind>, SourceReadError>;

    /// The names of every entry of `directory`; `None` when it cannot be
    /// listed.
    ///
    /// # Errors
    /// Returns [`SourceReadError::Unavailable`] when the reader cannot answer.
    fn directory_entries(&self, directory: &Path)
    -> Result<Option<Vec<OsString>>, SourceReadError>;

    /// The regular file `path`, opened without following a link or blocking:
    /// [`SourceFile::TooLarge`] when it is longer than `max_len` bytes,
    /// otherwise at most `limit + 1` of its bytes.
    ///
    /// # Errors
    /// Returns [`SourceReadError::Unavailable`] when the reader cannot answer.
    fn read_file(
        &self,
        path: &Path,
        max_len: u64,
        limit: usize,
    ) -> Result<SourceFile, SourceReadError>;
}

/// The reader of this process's own filesystem.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LocalInstructionSourceReader;

impl InstructionSourceReader for LocalInstructionSourceReader {
    fn canonicalize(&self, path: &Path) -> Result<SourceResolution, SourceReadError> {
        Ok(match std::fs::canonicalize(path) {
            Ok(path) => SourceResolution::Resolved(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => SourceResolution::NotFound,
            Err(_) => SourceResolution::Unresolvable,
        })
    }

    fn anchored_directory(&self, directory: &Path) -> Result<DirectoryAnswer, SourceReadError> {
        Ok(match open_absolute_directory(directory) {
            Err(refusal) => DirectoryAnswer::Refused(refusal),
            Ok(opened) => match opened.metadata() {
                Err(_) => DirectoryAnswer::Refused(DirectoryRefusal::Unreadable),
                Ok(metadata) if metadata.is_dir() => DirectoryAnswer::Directory,
                Ok(_) => DirectoryAnswer::NotDirectory,
            },
        })
    }

    fn read_instruction_chain(
        &self,
        directory: &Path,
        components: &[OsString],
        limit: usize,
    ) -> Result<Vec<InstructionChainEntry>, SourceReadError> {
        let mut entries = Vec::with_capacity(components.len() + 1);
        let mut opened = match open_absolute_directory(directory) {
            Ok(opened) => opened,
            Err(refusal) => {
                entries.push(InstructionChainEntry::Unopened(refusal));
                return Ok(entries);
            }
        };
        let mut physical_directory = directory.to_path_buf();
        for index in 0..=components.len() {
            entries.push(InstructionChainEntry::File(read_instruction_file(
                &opened, limit,
            )));
            if let Some(component) = components.get(index) {
                match open_child_directory(&opened, component, &physical_directory) {
                    Ok(child) => opened = child,
                    Err(refusal) => {
                        entries.push(InstructionChainEntry::Unopened(refusal));
                        return Ok(entries);
                    }
                }
                physical_directory.push(component);
            }
        }
        Ok(entries)
    }

    fn is_directory(&self, path: &Path) -> Result<bool, SourceReadError> {
        Ok(path.is_dir())
    }

    fn entry_kind(&self, path: &Path) -> Result<Option<SourceEntryKind>, SourceReadError> {
        let Ok(metadata) = path.symlink_metadata() else {
            return Ok(None);
        };
        Ok(Some(if metadata.is_symlink() {
            SourceEntryKind::Link
        } else if metadata.is_dir() {
            SourceEntryKind::Directory
        } else {
            SourceEntryKind::Other
        }))
    }

    fn directory_entries(
        &self,
        directory: &Path,
    ) -> Result<Option<Vec<OsString>>, SourceReadError> {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Ok(None);
        };
        Ok(entries
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()
            .ok())
    }

    fn read_file(
        &self,
        path: &Path,
        max_len: u64,
        limit: usize,
    ) -> Result<SourceFile, SourceReadError> {
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SourceFile::Missing);
            }
            Err(_) => return Ok(SourceFile::Unreadable),
            Ok(metadata) if metadata.is_symlink() => return Ok(SourceFile::Unsafe),
            Ok(metadata) if !metadata.is_file() => return Ok(SourceFile::NotRegular),
            Ok(_) => {}
        }
        let Ok(descriptor) = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            return Ok(SourceFile::Unsafe);
        };
        Ok(read_opened_file(
            File::from(descriptor),
            Some(max_len),
            limit,
        ))
    }
}

/// Reads the instruction file of an opened directory: no link, no blocking
/// open, at most `limit + 1` bytes.
fn read_instruction_file(directory: &File, limit: usize) -> SourceFile {
    let descriptor = match rustix::fs::openat(
        directory,
        INSTRUCTION_FILE_NAME,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => return SourceFile::Missing,
        Err(_) => return SourceFile::Unsafe,
    };
    read_opened_file(File::from(descriptor), None, limit)
}

fn read_opened_file(file: File, max_len: Option<u64>, limit: usize) -> SourceFile {
    let Ok(metadata) = file.metadata() else {
        return SourceFile::Unreadable;
    };
    if !metadata.is_file() {
        return SourceFile::NotRegular;
    }
    if max_len.is_some_and(|max_len| metadata.len() > max_len) {
        return SourceFile::TooLarge;
    }
    let mut bytes = Vec::new();
    let take = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    if file.take(take).read_to_end(&mut bytes).is_err() {
        return SourceFile::Unreadable;
    }
    SourceFile::Bytes(bytes)
}

fn open_absolute_directory(path: &Path) -> Result<File, DirectoryRefusal> {
    if !path.is_absolute() {
        return Err(DirectoryRefusal::Unsafe { path: path.into() });
    }
    let mut directory = File::open("/").map_err(|_| DirectoryRefusal::Unreadable)?;
    let mut traversed = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => {
                directory = open_child_directory(&directory, value, &traversed)?;
                traversed.push(value);
            }
            _ => return Err(DirectoryRefusal::Unsafe { path: path.into() }),
        }
    }
    Ok(directory)
}

fn open_child_directory(
    parent: &File,
    name: &OsStr,
    parent_path: &Path,
) -> Result<File, DirectoryRefusal> {
    rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| DirectoryRefusal::Unsafe {
        path: parent_path.join(name),
    })
}
