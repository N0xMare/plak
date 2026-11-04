//! The keys a 0.0.3 build refuses as unknown (`[host] resume_after_loss`,
//! `[mcp_servers.<id>] placement`, `[runners.<name>]`): accepted, validated,
//! and written only when set explicitly.

use std::path::Path;

use super::ProductConfig;
use super::keys::{ConfigField, ConfigKey, FieldValue, ResumeAfterLoss};
use crate::home::paths::PlakPaths;

impl ResumeAfterLoss {
    /// The value a file or `plak config set` gives the key; an empty
    /// command-line value clears it, as for `[root] agent`.
    pub(super) fn assigned(key: &ConfigKey, value: FieldValue<'_>) -> Result<Option<Self>, String> {
        let text = value.string(key)?;
        if matches!(value, FieldValue::Cli(_)) && text.is_empty() {
            return Ok(None);
        }
        Self::parse(text).map(Some)
    }
}

/// Which forward-only sections a parsed file spelled, whatever they held,
/// and the forward keys of a project file its overlay ignored.
#[derive(Clone, Debug, Default)]
pub(super) struct ForwardPresence {
    /// `host` and `runners`, when the file has the table at all.
    pub(super) sections: std::collections::BTreeSet<&'static str>,
    /// The project file's home-scoped forward keys, with its path.
    pub(super) project: Vec<String>,
}

impl ForwardPresence {
    /// The presence a parsed file records: whether it spelled `[host]` and
    /// `[runners]` at all.
    pub(super) fn of(host: bool, runners: bool) -> Self {
        let mut sections = std::collections::BTreeSet::new();
        for (name, present) in [("host", host), ("runners", runners)] {
            if present {
                sections.insert(name);
            }
        }
        Self {
            sections,
            project: Vec::new(),
        }
    }
}

impl ProductConfig {
    /// The keys this config's files carry that a 0.0.3 build refuses as
    /// unknown, in the words `plak doctor` names them: a set key, or a
    /// forward-only table that is present even when empty, in the home file
    /// or a trusted project file.
    #[must_use]
    pub fn forward_only_keys(&self) -> Vec<String> {
        let mut keys = self.forward_home_keys();
        for (id, server) in &self.tool_sections.mcp_servers {
            if server.placement.is_some() {
                keys.push(format!("[mcp_servers.{id}] placement"));
            }
        }
        keys.extend(self.forward.project.iter().cloned());
        keys
    }

    /// Warns once per home-scoped forward key the project file at `path`
    /// carries, and records it for doctor; the overlay ignores them.
    pub(super) fn warn_forward_home_scoped(
        &mut self,
        overlay: &Self,
        path: &Path,
        paths: &PlakPaths,
    ) {
        for key in overlay.forward_home_keys() {
            self.warnings.push(format!(
                "{}: {key} is home-scoped and ignored here; set it in {}",
                path.display(),
                paths.config_file().display(),
            ));
            self.forward
                .project
                .push(format!("{key} in {}", path.display()));
        }
    }

    /// The home-scoped forward keys and tables this file carries.
    pub(super) fn forward_home_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        if self.present.contains(&ConfigField::ResumeAfterLoss) {
            keys.push("[host] resume_after_loss".to_owned());
        } else if self.forward.sections.contains("host") {
            keys.push("[host]".to_owned());
        }
        if self.runners.is_empty() && self.forward.sections.contains("runners") {
            keys.push("[runners]".to_owned());
        }
        keys.extend(self.runners.keys().map(|name| format!("[runners.{name}]")));
        keys
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
