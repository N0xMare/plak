//! Retained bounded bytes: parsers never reread an unchecked pathname.

use super::super::{ReadRefusal, ReadScope, UnavailableCode, limits, unavailable};
use crate::home::paths::PlakPaths;
use rustix::fs::{Mode, OFlags};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read as _;

#[derive(Clone, Eq, PartialEq, Default)]
pub(super) struct Files {
    pub config: Option<String>,
    pub templates: BTreeMap<String, String>,
    pub profiles: BTreeMap<String, String>,
    pub unavailable: BTreeSet<String>,
}

struct Budget {
    bytes: usize,
    entries: usize,
}
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const READ: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);
fn failure(cause: UnavailableCode) -> ReadRefusal {
    unavailable(ReadScope::Config, cause)
}

impl Files {
    pub fn read(paths: &PlakPaths) -> Result<Self, ReadRefusal> {
        let home = match rustix::fs::open(paths.home(), DIRECTORY, Mode::empty()) {
            Ok(home) => home,
            Err(rustix::io::Errno::NOENT) => return Ok(Self::default()),
            Err(_) => return Err(failure(UnavailableCode::Storage)),
        };
        let mut budget = Budget {
            bytes: 0,
            entries: 0,
        };
        let config = read_file(&home, "config.toml", &mut budget)?;
        if config.is_some() {
            budget.entries = 1;
        }
        let mut unavailable = BTreeSet::new();
        let templates = read_directory(&home, "agents", &mut budget, &mut unavailable)?;
        let profiles = read_directory(&home, "profiles", &mut budget, &mut unavailable)?;
        Ok(Self {
            config,
            templates,
            profiles,
            unavailable,
        })
    }

    pub fn hash_into(&self, hash: &mut impl sha2::Digest) {
        for name in &self.unavailable {
            hash.update(b"unavailable");
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
        }
        for (kind, files) in [
            (b"agents".as_slice(), &self.templates),
            (b"profiles".as_slice(), &self.profiles),
        ] {
            hash.update(kind);
            for (name, body) in files {
                hash.update((name.len() as u64).to_le_bytes());
                hash.update(name.as_bytes());
                hash.update((body.len() as u64).to_le_bytes());
                hash.update(body.as_bytes());
            }
        }
        match &self.config {
            Some(config) => {
                hash.update(b"config:present");
                hash.update(config.as_bytes());
            }
            None => hash.update(b"config:absent"),
        }
    }
}

fn read_file(
    directory: &impl std::os::fd::AsFd,
    name: &str,
    budget: &mut Budget,
) -> Result<Option<String>, ReadRefusal> {
    let file = match rustix::fs::openat(directory.as_fd(), name, READ, Mode::empty()) {
        Ok(file) => File::from(file),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(failure(UnavailableCode::Storage)),
    };
    if !file
        .metadata()
        .map_err(|_| failure(UnavailableCode::Storage))?
        .is_file()
    {
        return Err(failure(UnavailableCode::Storage));
    }
    let mut bytes = Vec::new();
    let allowed =
        limits::DEFINITION_BYTES.min(limits::DEPENDENCY_BYTES.saturating_sub(budget.bytes));
    let read = file.take((allowed + 1) as u64).read_to_end(&mut bytes);
    budget.bytes = budget
        .bytes
        .checked_add(bytes.len())
        .ok_or_else(|| failure(UnavailableCode::DependencyLimit))?;
    if budget.bytes > limits::DEPENDENCY_BYTES {
        return Err(failure(UnavailableCode::DependencyLimit));
    }
    read.map_err(|_| failure(UnavailableCode::Storage))?;
    if bytes.len() > limits::DEFINITION_BYTES {
        return Err(failure(UnavailableCode::DefinitionTooLarge));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| failure(UnavailableCode::Configuration))
}

fn read_directory(
    home: &impl std::os::fd::AsFd,
    name: &str,
    budget: &mut Budget,
    unavailable: &mut BTreeSet<String>,
) -> Result<BTreeMap<String, String>, ReadRefusal> {
    let directory = match rustix::fs::openat(home.as_fd(), name, DIRECTORY, Mode::empty()) {
        Ok(directory) => directory,
        Err(rustix::io::Errno::NOENT) => return Ok(BTreeMap::new()),
        Err(_) => {
            unavailable.insert(name.to_owned());
            return Ok(BTreeMap::new());
        }
    };
    let Ok(entries) = rustix::fs::Dir::read_from(&directory) else {
        unavailable.insert(name.to_owned());
        return Ok(BTreeMap::new());
    };
    let mut files = BTreeMap::new();
    for entry in entries {
        let Ok(entry) = entry else {
            unavailable.insert(name.to_owned());
            break;
        };
        let raw = entry.file_name().to_bytes();
        if raw == b"." || raw == b".." {
            continue;
        }
        budget.entries += 1;
        if budget.entries > limits::SCAN_ENTRIES {
            return Err(failure(UnavailableCode::DependencyLimit));
        }
        let Ok(filename) = std::str::from_utf8(raw) else {
            continue;
        };
        let Some(stem) = filename.strip_suffix(".toml") else {
            continue;
        };
        if name == "agents" && stem.starts_with('.') {
            continue;
        }
        if stem.is_empty()
            || stem.len() > 128
            || stem.contains(['/', '\\', '\0'])
            || stem.chars().any(char::is_whitespace)
        {
            continue;
        }
        match read_file(&directory, filename, budget) {
            Ok(Some(body)) => {
                files.insert(stem.to_owned(), body);
            }
            Err(
                error @ ReadRefusal::Unavailable {
                    cause: UnavailableCode::DependencyLimit,
                    ..
                },
            ) => return Err(error),
            Ok(None) | Err(_) => {
                unavailable.insert(stem.to_owned());
            }
        }
    }
    Ok(files)
}
