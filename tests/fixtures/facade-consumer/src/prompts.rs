//! A consumer-implemented instruction source reader drives `AGENTS.md` and
//! Agent Skills discovery through the facade.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use plakous::prompts::instructions::{
    DirectoryAnswer, DirectoryRefusal, InstructionChainEntry, InstructionSourceReader,
    SourceEntryKind, SourceFile, SourceReadError, SourceResolution, discover_instruction_sources,
};
use plakous::prompts::skills::{SkillRoot, discover_skills};

/// Files that exist only in memory, under absolute paths no host has.
struct MemoryReader {
    files: BTreeMap<PathBuf, &'static str>,
}

impl MemoryReader {
    fn new() -> Self {
        let files = BTreeMap::from([
            (PathBuf::from("/memory/repo/AGENTS.md"), "remote rules"),
            (
                PathBuf::from("/memory/repo/.agents/skills/deploy/SKILL.md"),
                "---\nname: deploy\ndescription: Ship it.\n---\nSteps.\n",
            ),
        ]);
        Self { files }
    }

    fn is_directory_path(&self, path: &Path) -> bool {
        self.files
            .keys()
            .any(|file| file.starts_with(path) && file != path)
    }
}

impl InstructionSourceReader for MemoryReader {
    fn canonicalize(&self, path: &Path) -> Result<SourceResolution, SourceReadError> {
        Ok(
            if self.files.contains_key(path) || self.is_directory_path(path) {
                SourceResolution::Resolved(path.to_path_buf())
            } else {
                SourceResolution::NotFound
            },
        )
    }

    fn anchored_directory(&self, directory: &Path) -> Result<DirectoryAnswer, SourceReadError> {
        Ok(if self.is_directory_path(directory) {
            DirectoryAnswer::Directory
        } else {
            DirectoryAnswer::Refused(DirectoryRefusal::Unsafe {
                path: directory.to_path_buf(),
            })
        })
    }

    fn read_instruction_chain(
        &self,
        directory: &Path,
        components: &[OsString],
        limit: usize,
    ) -> Result<Vec<InstructionChainEntry>, SourceReadError> {
        let mut current = directory.to_path_buf();
        let mut entries = Vec::new();
        for index in 0..=components.len() {
            let file = self.read_file(&current.join("AGENTS.md"), u64::MAX, limit)?;
            entries.push(InstructionChainEntry::File(file));
            if let Some(component) = components.get(index) {
                current.push(component);
            }
        }
        Ok(entries)
    }

    fn is_directory(&self, path: &Path) -> Result<bool, SourceReadError> {
        Ok(self.is_directory_path(path))
    }

    fn entry_kind(&self, path: &Path) -> Result<Option<SourceEntryKind>, SourceReadError> {
        Ok(if self.is_directory_path(path) {
            Some(SourceEntryKind::Directory)
        } else if self.files.contains_key(path) {
            Some(SourceEntryKind::Other)
        } else {
            None
        })
    }

    fn directory_entries(
        &self,
        directory: &Path,
    ) -> Result<Option<Vec<OsString>>, SourceReadError> {
        let mut names = self
            .files
            .keys()
            .filter_map(|file| file.strip_prefix(directory).ok())
            .filter_map(|relative| relative.components().next())
            .map(|component| component.as_os_str().to_os_string())
            .collect::<Vec<_>>();
        names.dedup();
        Ok(Some(names))
    }

    fn read_file(
        &self,
        path: &Path,
        _max_len: u64,
        limit: usize,
    ) -> Result<SourceFile, SourceReadError> {
        Ok(match self.files.get(path) {
            Some(content) => SourceFile::Bytes(
                content.as_bytes()[..content.len().min(limit + 1)].to_vec(),
            ),
            None => SourceFile::Missing,
        })
    }
}

pub(crate) fn consumer_reader_drives_discovery() -> Result<(), Box<dyn std::error::Error>> {
    let reader = MemoryReader::new();
    let root = Path::new("/memory/repo");
    let instructions = discover_instruction_sources(&reader, None, root, root)?;
    let [source] = instructions.sources() else {
        return Err("one remote instruction source".into());
    };
    assert_eq!(
        source.registered_source()?.content(),
        "remote rules",
        "the consumer's reader supplied the bytes"
    );
    let skills = discover_skills(&reader, root, &[SkillRoot::project(root)])?;
    let [skill] = skills.skills() else {
        return Err("one remote skill".into());
    };
    assert_eq!(skill.name(), "deploy");
    let loaded = skill.load(&reader).map_err(|diagnostic| diagnostic.to_string())?;
    assert_eq!(loaded.body().trim(), "Steps.");
    Ok(())
}
