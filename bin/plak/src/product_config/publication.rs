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
