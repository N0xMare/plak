use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

pub const MAX_CHANGES_ENTRIES: usize = 2_000;
/// Internal status JSON budget, separate from the client projection's 256 KiB.
/// Fits 2,000 entries with 4,096-byte paths and maximal object/content witnesses,
/// leaving 4 MiB below the runner frame limit. JSON escaping counts toward it.
pub const MAX_CHANGES_STATUS_BYTES: usize = 12 * 1024 * 1024;
pub const MAX_CHANGES_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_CHANGES_INPUT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangesRefusal {
    NotARepository,
    VcsUnavailable,
    VcsUnsupported,
    ProtectedPath,
    UnsafeRepository,
    InvalidRequest,
    Conflict,
    TooLarge,
    OutputLimit,
    Deadline,
    Cancelled,
    Io,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangesRepository {
    pub workspace: PathBuf,
    pub toplevel: PathBuf,
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
    pub identity: String,
    pub git_view: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangesObservation {
    pub head: Option<String>,
    pub index: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChangesContent {
    Missing,
    File { size: u64, digest: String },
    TooLarge { size: u64 },
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangesEntry {
    pub path: String,
    /// The exact two-byte porcelain index/worktree state, or `??`.
    pub state: String,
    pub head_object: Option<String>,
    pub index_object: Option<String>,
    pub content: ChangesContent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangesStatus {
    pub repository: ChangesRepository,
    pub observation: ChangesObservation,
    pub base_label: String,
    pub entries: Vec<ChangesEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChangesDiff {
    Patch { text: String },
    Binary { numstat: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "answer",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ChangesAnswer {
    Status(ChangesStatus),
    Diff(ChangesDiff),
    Refused(ChangesRefusal),
}

impl ChangesAnswer {
    pub(crate) fn validate(&self) -> Result<(), ChangesRefusal> {
        match self {
            Self::Refused(_) => Ok(()),
            Self::Diff(ChangesDiff::Patch { text } | ChangesDiff::Binary { numstat: text }) => {
                if text.len() > MAX_CHANGES_OUTPUT_BYTES {
                    return Err(ChangesRefusal::OutputLimit);
                }
                Ok(())
            }
            Self::Status(status) => {
                if status.entries.len() > MAX_CHANGES_ENTRIES
                    || status.base_label.len() > 4096
                    || serde_json::to_vec(status)
                        .map_err(|_| ChangesRefusal::InvalidRequest)?
                        .len()
                        > MAX_CHANGES_STATUS_BYTES
                {
                    return Err(ChangesRefusal::OutputLimit);
                }
                ChangesRequest::diff(
                    status.repository.clone(),
                    status.observation.clone(),
                    ChangesEntry {
                        path: ".".into(),
                        state: "??".into(),
                        head_object: None,
                        index_object: None,
                        content: ChangesContent::Missing,
                    },
                )?;
                for entry in &status.entries {
                    ChangesRequest::diff(
                        status.repository.clone(),
                        status.observation.clone(),
                        entry.clone(),
                    )?;
                }
                Ok(())
            }
        }
    }
}

/// A validated closed status/diff request. Only trusted placement metadata may
/// supply the optional containing repository, never a browser request.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct ChangesRequest(pub(super) Request);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "query", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Request {
    Status {
        repository: Option<PathBuf>,
    },
    Diff {
        repository: ChangesRepository,
        observation: ChangesObservation,
        entry: Box<ChangesEntry>,
    },
}

impl ChangesRequest {
    #[must_use]
    pub const fn status() -> Self {
        Self(Request::Status { repository: None })
    }

    pub fn status_in_repository(toplevel: PathBuf) -> Result<Self, ChangesRefusal> {
        absolute(&toplevel)?;
        Ok(Self(Request::Status {
            repository: Some(toplevel),
        }))
    }

    pub fn diff(
        repository: ChangesRepository,
        observation: ChangesObservation,
        entry: ChangesEntry,
    ) -> Result<Self, ChangesRefusal> {
        for path in [
            &repository.workspace,
            &repository.toplevel,
            &repository.git_dir,
            &repository.common_dir,
        ] {
            absolute(path)?;
        }
        relative(&entry.path)?;
        if !repository.workspace.starts_with(&repository.toplevel)
            || !digest(&repository.identity)
            || !digest(&observation.index)
            || !observation.head.as_deref().is_none_or(object)
            || !entry.head_object.as_deref().is_none_or(object)
            || !entry.index_object.as_deref().is_none_or(object)
            || entry.state.len() != 2
            || !entry
                .state
                .bytes()
                .all(|byte| b".MADRCUT?!".contains(&byte))
            || matches!(&entry.content, ChangesContent::File { size, digest: hash }
                if *size > MAX_CHANGES_INPUT_BYTES || !digest(hash))
        {
            return Err(ChangesRefusal::InvalidRequest);
        }
        Ok(Self(Request::Diff {
            repository,
            observation,
            entry: Box::new(entry),
        }))
    }
}

impl<'de> Deserialize<'de> for ChangesRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let request = Request::deserialize(deserializer)?;
        match request {
            Request::Status { repository: None } => Ok(Self::status()),
            Request::Status {
                repository: Some(path),
            } => Self::status_in_repository(path),
            Request::Diff {
                repository,
                observation,
                entry,
            } => Self::diff(repository, observation, *entry),
        }
        .map_err(|_| serde::de::Error::custom("invalid changes request"))
    }
}

pub(super) fn relative(path: &str) -> Result<(), ChangesRefusal> {
    // Porcelain's normal untracked mode may collapse the selected workspace
    // itself to one directory entry. It is listable, never diffable.
    if path == "." {
        return Ok(());
    }
    if path.is_empty()
        || path.len() > 4096
        || path.contains('\0')
        || Path::new(path)
            .components()
            .any(|part| !matches!(part, Component::Normal(name) if name != ".git" && name != ".jj"))
    {
        return Err(ChangesRefusal::InvalidRequest);
    }
    Ok(())
}

fn absolute(path: &Path) -> Result<(), ChangesRefusal> {
    if !path.is_absolute()
        || path
            .to_str()
            .is_none_or(|path| path.len() > 4096 || path.contains('\0'))
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(ChangesRefusal::InvalidRequest);
    }
    Ok(())
}

pub(super) fn object(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
