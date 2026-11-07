//! Serialize config publication and merge each settings writer's own changes.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt as _;

use super::{ConfigSaveError, ConfigWriteScope, ProductConfig, keys::CONFIG_KEYS};
use crate::home::paths::PlakPaths;

pub(super) struct ConfigLock {
    _file: File,
}

impl ConfigLock {
    pub(super) fn take(paths: &PlakPaths) -> Result<Self, ConfigSaveError> {
        let failed = |message: &str| ConfigSaveError::BeforePublication(message.to_owned());
        paths
            .ensure_layout()
            .map_err(ConfigSaveError::BeforePublication)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(
                i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits())
                    .map_err(|_| failed("invalid config publication lock flags"))?,
            )
            .open(paths.home().join("config.lock"))
            .map_err(|_| failed("could not open config publication lock"))?;
        file.lock()
            .map_err(|_| failed("could not take config publication lock"))?;
        Ok(Self { _file: file })
    }
}

impl ProductConfig {
    /// User-scope save. When project sources contributed to this merged
    /// config, the global file still receives only global content: each
    /// project-origin key keeps its baseline value and project templates are
    /// not written into `~/.plak/agents`.
    pub fn save_publication_aware(&self, paths: &PlakPaths) -> Result<(), ConfigSaveError> {
        let _lock = ConfigLock::take(paths)?;
        let baseline = Self::load_config_file(&paths.config_file())
            .map_err(ConfigSaveError::BeforePublication)?;
        self.preserving_terminal_preferences(&baseline)
            .save_user_unlocked(paths)
    }

    /// Project-scope save: `<project>/.plak/config.toml` carries only the
    /// keys whose effective value differs from the global baseline, and
    /// `.plak/agents/` gains only the templates that differ from (or are
    /// absent from) the user directory. Requires a trusted project.
    pub fn save_project_publication_aware(&self, paths: &PlakPaths) -> Result<(), ConfigSaveError> {
        let _lock = ConfigLock::take(paths)?;
        let baseline = Self::load(paths).map_err(ConfigSaveError::BeforePublication)?;
        self.preserving_terminal_preferences(&baseline)
            .save_project_with_fault(paths, &mut |_, _| Ok(()))
    }

    fn preserving_terminal_preferences(&self, baseline: &Self) -> Self {
        let mut config = self.clone();
        for row in CONFIG_KEYS.iter().filter(|row| row.section() == "tui") {
            config.assign_overlay_field(row.field, baseline);
            config.set_origin(row.field, baseline.origin(row.field));
        }
        config
    }

    /// An explicit terminal-preference key edit, not a whole product snapshot.
    pub(crate) fn save_terminal_key(
        paths: &PlakPaths,
        scope: ConfigWriteScope,
        key: &str,
        value: &str,
    ) -> Result<(), ConfigSaveError> {
        let _lock = ConfigLock::take(paths)?;
        let mut config = match scope {
            ConfigWriteScope::User => Self::load_user(paths),
            ConfigWriteScope::Project => Self::load(paths),
        }
        .map_err(ConfigSaveError::BeforePublication)?;
        config
            .set_key(key, value)
            .map_err(ConfigSaveError::BeforePublication)?;
        match scope {
            ConfigWriteScope::User => config.save_user_unlocked(paths),
            ConfigWriteScope::Project => config.save_project_with_fault(paths, &mut |_, _| Ok(())),
        }
    }

    /// Changes home-only preferences against a fresh baseline under the lock.
    pub(crate) fn update_user(
        paths: &PlakPaths,
        update: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<Self, ConfigSaveError> {
        let _lock = ConfigLock::take(paths)?;
        let mut config = Self::load_user(paths).map_err(ConfigSaveError::BeforePublication)?;
        update(&mut config).map_err(ConfigSaveError::BeforePublication)?;
        config.save_user_unlocked(paths)?;
        Ok(config)
    }

    /// Publishes only this host settings edit, never terminal preferences or
    /// another face's unchanged product fields.
    pub(crate) fn save_settings_delta(
        &self,
        paths: &PlakPaths,
        before: &Self,
        scope: ConfigWriteScope,
    ) -> Result<Self, ConfigSaveError> {
        let _lock = ConfigLock::take(paths)?;
        let mut config = Self::load(paths).map_err(ConfigSaveError::BeforePublication)?;
        for row in CONFIG_KEYS.iter().filter(|row| row.section() != "tui") {
            if !super::toml_file::same_value_opt(
                self.field_value(row.field).as_ref(),
                before.field_value(row.field).as_ref(),
            ) {
                config.assign_overlay_field(row.field, self);
                config.set_origin(row.field, self.origin(row.field));
            }
        }
        for (name, template) in &self.templates {
            if before.templates.get(name) != Some(template) {
                config.templates.insert(name.clone(), template.clone());
                config
                    .template_origins
                    .insert(name.clone(), self.template_origin(name));
            }
        }
        match scope {
            ConfigWriteScope::User => config.save_user_unlocked(paths)?,
            ConfigWriteScope::Project => {
                config.save_project_with_fault(paths, &mut |_, _| Ok(()))?;
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests;
