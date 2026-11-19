//! The keys a 0.0.3 build refuses as unknown (`[host] resume_after_loss`,
//! `[mcp_servers.<id>] placement`, `[runners.<name>]`, `[web]`): accepted,
//! validated, and written only when set explicitly.

use std::path::Path;

use super::ProductConfig;
use super::keys::{ConfigField, ConfigKey, FieldValue, ResumeAfterLoss};
use crate::home::paths::PlakPaths;

impl ResumeAfterLoss {
    /// The value a file or `plakd config set` gives the key; an empty
    /// command-line value clears it, as for `[root] agent`.
    pub(super) fn assigned(key: &ConfigKey, value: FieldValue<'_>) -> Result<Option<Self>, String> {
        let text = value.string(key)?;
        if matches!(value, FieldValue::Cli(_)) && text.is_empty() {
            return Ok(None);
        }
        Self::parse(text).map(Some)
    }
}

/// Which home-scoped sections a parsed file spelled, even when empty.
#[derive(Clone, Debug, Default)]
pub(super) struct ForwardPresence {
    /// `host`, `runners` and `web`, when the file has the table at all.
    pub(super) sections: std::collections::BTreeSet<&'static str>,
}

impl ForwardPresence {
    /// The presence a parsed file records: whether it spelled `[host]`,
    /// `[runners]` and `[web]` at all.
    pub(super) fn of(named: &[(&'static str, bool)]) -> Self {
        let mut sections = std::collections::BTreeSet::new();
        for (name, present) in named {
            if *present {
                sections.insert(*name);
            }
        }
        Self { sections }
    }
}

impl ProductConfig {
    /// Warns once per home-scoped key the project file at `path` carries;
    /// the overlay ignores them.
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
        let web = [
            (ConfigField::WebAllow, "[web] allow"),
            (ConfigField::WebOrigin, "[web] origin"),
            (ConfigField::WebAccountsRead, "[web] accounts_read"),
            (ConfigField::WebCredentialsWrite, "[web] credentials_write"),
            (ConfigField::WebConfigRead, "[web] config_read"),
            (ConfigField::WebConfigWrite, "[web] config_write"),
            (ConfigField::WebFilesRead, "[web] files_read"),
            (ConfigField::WebTerminal, "[web] terminal"),
        ];
        let set = web.iter().filter(|(field, _)| self.present.contains(field));
        let before = keys.len();
        keys.extend(set.map(|(_, key)| (*key).to_owned()));
        if keys.len() == before && self.forward.sections.contains("web") {
            keys.push("[web]".to_owned());
        }
        let gateway = [
            (ConfigField::GatewayAddress, "[gateway] address"),
            (ConfigField::GatewayKeyFile, "[gateway] key_file"),
            (
                ConfigField::GatewayAllowNonTailnet,
                "[gateway] allow_non_tailnet",
            ),
        ];
        let set = gateway
            .iter()
            .filter(|(field, _)| self.present.contains(field));
        let before = keys.len();
        keys.extend(set.map(|(_, key)| (*key).to_owned()));
        if keys.len() == before && self.forward.sections.contains("gateway") {
            keys.push("[gateway]".to_owned());
        }
        keys
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
