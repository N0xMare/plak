//! Ephemeral host mirrors of runner-owned pin metadata; never a store encoding.

use super::{Path, PathBuf};
use plakous::runner::protocol::operator::{OperatorNamespace, TerminalKey};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TerminalPin {
    pub host_generation: u64,
    pub host_id: String,
    pub generation: String,
    pub namespace: OperatorNamespace,
    pub runner: String,
    pub instance: String,
    pub key: TerminalKey,
    pub workspace: PathBuf,
    pub digest: String,
}

impl TerminalPin {
    pub(super) fn identity(&self) -> Result<String, String> {
        serde_json::to_string(&(
            &self.host_id,
            &self.generation,
            &self.namespace,
            &self.runner,
            &self.key,
        ))
        .map_err(|_| "invalid terminal pin identity".into())
    }
}

pub(super) fn physical(runner: &str, path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err("invalid workspace spelling".into());
    }
    if runner != "local" {
        return Ok(path.to_owned());
    }
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for name in missing.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name().ok_or("workspace has no name")?);
                ancestor = ancestor.parent().ok_or("workspace has no parent")?;
            }
            Err(_) => return Err("workspace identity unavailable".into()),
        }
    }
}
