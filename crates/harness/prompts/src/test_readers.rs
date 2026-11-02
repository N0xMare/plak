//! Readers for discovery tests.
//!
//! [`RemappedReader`] serves a workspace the host cannot see: every path
//! under `/runner` is answered from a local directory, and no other path
//! resolves. [`ScriptedReader`] wraps the local reader and breaks one part of
//! the contract, so discovery's own validation is what the tests observe.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::instructions::{
    DirectoryAnswer, DirectoryRefusal, InstructionChainEntry, InstructionSourceReader,
    LocalInstructionSourceReader, SourceEntryKind, SourceFile, SourceReadError, SourceResolution,
};

pub(crate) const RUNNER_ROOT: &str = "/runner";

pub(crate) struct RemappedReader {
    backing: PathBuf,
}

impl RemappedReader {
    /// `backing` must be canonical, so resolved paths map back exactly.
    pub(crate) fn new(backing: &Path) -> Self {
        Self {
            backing: std::fs::canonicalize(backing).unwrap(),
        }
    }

    fn local(&self, path: &Path) -> Option<PathBuf> {
        path.strip_prefix(RUNNER_ROOT)
            .ok()
            .map(|relative| self.backing.join(relative))
    }

    fn remote(&self, path: PathBuf) -> PathBuf {
        match path.strip_prefix(&self.backing).map(Path::to_path_buf) {
            Ok(relative) => Path::new(RUNNER_ROOT).join(relative),
            Err(_) => path,
        }
    }

    fn remote_refusal(&self, refusal: DirectoryRefusal) -> DirectoryRefusal {
        match refusal {
            DirectoryRefusal::Unsafe { path } => DirectoryRefusal::Unsafe {
                path: self.remote(path),
            },
            DirectoryRefusal::Unreadable => DirectoryRefusal::Unreadable,
        }
    }
}

impl InstructionSourceReader for RemappedReader {
    fn canonicalize(&self, path: &Path) -> Result<SourceResolution, SourceReadError> {
        let Some(local) = self.local(path) else {
            return Ok(SourceResolution::NotFound);
        };
        Ok(match LocalInstructionSourceReader.canonicalize(&local)? {
            SourceResolution::Resolved(canonical) if canonical.starts_with(&self.backing) => {
                SourceResolution::Resolved(self.remote(canonical))
            }
            SourceResolution::Resolved(_) => SourceResolution::Unresolvable,
            other => other,
        })
    }

    fn anchored_directory(&self, directory: &Path) -> Result<DirectoryAnswer, SourceReadError> {
        let Some(local) = self.local(directory) else {
            return Ok(DirectoryAnswer::Refused(DirectoryRefusal::Unsafe {
                path: directory.into(),
            }));
        };
        Ok(
            match LocalInstructionSourceReader.anchored_directory(&local)? {
                DirectoryAnswer::Refused(refusal) => {
                    DirectoryAnswer::Refused(self.remote_refusal(refusal))
                }
                other => other,
            },
        )
    }

    fn read_instruction_chain(
        &self,
        directory: &Path,
        components: &[OsString],
        limit: usize,
    ) -> Result<Vec<InstructionChainEntry>, SourceReadError> {
        let Some(local) = self.local(directory) else {
            return Ok(vec![InstructionChainEntry::Unopened(
                DirectoryRefusal::Unsafe {
                    path: directory.into(),
                },
            )]);
        };
        Ok(LocalInstructionSourceReader
            .read_instruction_chain(&local, components, limit)?
            .into_iter()
            .map(|entry| match entry {
                InstructionChainEntry::Unopened(refusal) => {
                    InstructionChainEntry::Unopened(self.remote_refusal(refusal))
                }
                file @ InstructionChainEntry::File(_) => file,
            })
            .collect())
    }

    fn is_directory(&self, path: &Path) -> Result<bool, SourceReadError> {
        Ok(self.local(path).is_some_and(|local| local.is_dir()))
    }

    fn entry_kind(&self, path: &Path) -> Result<Option<SourceEntryKind>, SourceReadError> {
        match self.local(path) {
            Some(local) => LocalInstructionSourceReader.entry_kind(&local),
            None => Ok(None),
        }
    }

    fn directory_entries(
        &self,
        directory: &Path,
    ) -> Result<Option<Vec<OsString>>, SourceReadError> {
        match self.local(directory) {
            Some(local) => LocalInstructionSourceReader.directory_entries(&local),
            None => Ok(None),
        }
    }

    fn read_file(
        &self,
        path: &Path,
        max_len: u64,
        limit: usize,
    ) -> Result<SourceFile, SourceReadError> {
        match self.local(path) {
            Some(local) => LocalInstructionSourceReader.read_file(&local, max_len, limit),
            None => Ok(SourceFile::Missing),
        }
    }
}

/// One way a reader breaks its contract.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Misbehavior {
    /// Every answer is `Unavailable`.
    Unavailable,
    /// Every chain file is exactly `limit` bytes, so the files together
    /// exceed the budget although each read keeps its limit.
    IgnoresBudget,
    /// Every read returns `limit + 2` bytes, more than any read admits.
    OverReads,
    /// The chain repeats its first entry.
    DuplicatesEntries,
    /// The chain drops its last entry.
    DropsEntry,
    /// Directory listings repeat every name and add one that escapes.
    MalformedListing,
}

/// The local reader with one [`Misbehavior`].
pub(crate) struct ScriptedReader(pub(crate) Misbehavior);

impl ScriptedReader {
    const fn available(&self) -> Result<(), SourceReadError> {
        match self.0 {
            Misbehavior::Unavailable => Err(SourceReadError::Unavailable),
            _ => Ok(()),
        }
    }
}

impl InstructionSourceReader for ScriptedReader {
    fn canonicalize(&self, path: &Path) -> Result<SourceResolution, SourceReadError> {
        self.available()?;
        LocalInstructionSourceReader.canonicalize(path)
    }

    fn anchored_directory(&self, directory: &Path) -> Result<DirectoryAnswer, SourceReadError> {
        self.available()?;
        LocalInstructionSourceReader.anchored_directory(directory)
    }

    fn read_instruction_chain(
        &self,
        directory: &Path,
        components: &[OsString],
        limit: usize,
    ) -> Result<Vec<InstructionChainEntry>, SourceReadError> {
        self.available()?;
        let mut entries =
            LocalInstructionSourceReader.read_instruction_chain(directory, components, limit)?;
        match self.0 {
            Misbehavior::IgnoresBudget | Misbehavior::OverReads => {
                let length = match self.0 {
                    Misbehavior::OverReads => limit + 2,
                    _ => limit,
                };
                for entry in &mut entries {
                    *entry = InstructionChainEntry::File(SourceFile::Bytes(vec![b'x'; length]));
                }
            }
            Misbehavior::DuplicatesEntries => {
                let first = entries[0].clone();
                entries.insert(0, first);
            }
            Misbehavior::DropsEntry => {
                entries.pop();
            }
            Misbehavior::Unavailable | Misbehavior::MalformedListing => {}
        }
        Ok(entries)
    }

    fn is_directory(&self, path: &Path) -> Result<bool, SourceReadError> {
        self.available()?;
        LocalInstructionSourceReader.is_directory(path)
    }

    fn entry_kind(&self, path: &Path) -> Result<Option<SourceEntryKind>, SourceReadError> {
        self.available()?;
        LocalInstructionSourceReader.entry_kind(path)
    }

    fn directory_entries(
        &self,
        directory: &Path,
    ) -> Result<Option<Vec<OsString>>, SourceReadError> {
        self.available()?;
        let entries = LocalInstructionSourceReader.directory_entries(directory)?;
        Ok(match self.0 {
            Misbehavior::MalformedListing => entries.map(|mut names| {
                names.extend(names.clone());
                names.push(OsString::from("../escape"));
                names
            }),
            _ => entries,
        })
    }

    fn read_file(
        &self,
        path: &Path,
        max_len: u64,
        limit: usize,
    ) -> Result<SourceFile, SourceReadError> {
        self.available()?;
        match self.0 {
            Misbehavior::OverReads => Ok(SourceFile::Bytes(vec![b'x'; limit + 2])),
            _ => LocalInstructionSourceReader.read_file(path, max_len, limit),
        }
    }
}
