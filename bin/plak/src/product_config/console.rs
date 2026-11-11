//! Home projections and guarded publication for the operator console.

use super::{ConfigField, ConfigValueOrigin, ProductConfig, parse_template};
use std::collections::BTreeMap;

pub(crate) fn with_write_lock<T>(
    paths: &crate::home::paths::PlakPaths,
    work: impl FnOnce() -> T,
) -> Result<T, super::ConfigSaveError> {
    let _guard = super::publication::ConfigLock::try_take(paths)?;
    Ok(work())
}

pub(crate) fn edit_config(
    existing: Option<&str>,
    candidate: &ProductConfig,
) -> Result<String, String> {
    super::toml_file::edit_document(Some(existing.unwrap_or_default()), candidate)
}

pub(crate) enum DefinitionFile<'a> {
    Config,
    Template(&'a str),
    Profile(&'a str),
}

pub(crate) fn publish(
    paths: &crate::home::paths::PlakPaths,
    file: DefinitionFile<'_>,
    bytes: &[u8],
    create_new: bool,
) -> Result<(), super::ConfigSaveError> {
    publish_with_fault(paths, file, bytes, create_new, &mut |_, _| Ok(()))
}

fn publish_with_fault(
    paths: &crate::home::paths::PlakPaths,
    file: DefinitionFile<'_>,
    bytes: &[u8],
    create_new: bool,
    fault: &mut impl FnMut(&std::path::Path, super::atomic::AtomicStage) -> Result<(), String>,
) -> Result<(), super::ConfigSaveError> {
    use rustix::fs::{Mode, OFlags};
    let before = || {
        super::ConfigSaveError::BeforePublication(
            "unsafe or unavailable home publication directory".into(),
        )
    };
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let home = rustix::fs::open(paths.home(), flags, Mode::empty()).map_err(|_| before())?;
    let stat = rustix::fs::fstat(&home).map_err(|_| before())?;
    if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
        return Err(before());
    }
    let (directory, target) = match file {
        DefinitionFile::Config => (home, paths.config_file()),
        DefinitionFile::Template(id) | DefinitionFile::Profile(id) => {
            super::validate_template_name(id).map_err(|_| before())?;
            let name = if matches!(file, DefinitionFile::Template(_)) {
                "agents"
            } else {
                "profiles"
            };
            match rustix::fs::mkdirat(&home, name, Mode::RWXU) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(_) => return Err(before()),
            }
            let directory =
                rustix::fs::openat(&home, name, flags, Mode::empty()).map_err(|_| before())?;
            rustix::fs::fsync(&home).map_err(|_| before())?;
            (
                directory,
                paths.home().join(name).join(format!("{id}.toml")),
            )
        }
    };
    let stat = rustix::fs::fstat(&directory).map_err(|_| before())?;
    if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
        return Err(before());
    }
    super::atomic::atomic_file_at(&directory, &target, bytes, create_new, fault)
}

impl ProductConfig {
    pub(crate) fn apply_console_changes(
        &mut self,
        changes: &[plakd_wire_types::console::ConfigChange],
    ) -> Result<(), String> {
        use plakd_wire_types::console::SettingValue;
        let mut candidate = self.clone();
        // Root selection clears an inherited composition before an explicitly
        // supplied composition in the same complete candidate is assigned.
        for change in changes.iter().filter(|change| change.key == "root.agent") {
            match &change.value {
                SettingValue::Null => candidate.clear_root_binding(),
                SettingValue::String(value) => candidate.assign_field(
                    super::config_key("root.agent").ok_or("unknown key")?,
                    super::keys::FieldValue::Toml(&toml::Value::String(value.clone())),
                )?,
                _ => return Err("invalid root agent value".into()),
            }
        }
        if candidate.root_agent != self.root_agent {
            candidate.root_composition = None;
        }
        for change in changes.iter().filter(|change| change.key != "root.agent") {
            let key = super::config_key(&change.key).ok_or("unknown key")?;
            if matches!(change.value, SettingValue::Null)
                && key.field == ConfigField::RootComposition
            {
                candidate.root_composition = None;
                continue;
            }
            let value = match &change.value {
                SettingValue::Bool(value) => toml::Value::Boolean(*value),
                SettingValue::Integer(value) => toml::Value::Integer(*value),
                SettingValue::String(value) => toml::Value::String(value.clone()),
                SettingValue::Strings(value) => {
                    toml::Value::Array(value.iter().cloned().map(toml::Value::String).collect())
                }
                SettingValue::Null => return Err("this setting cannot be null".into()),
            };
            candidate.assign_field(key, super::keys::FieldValue::Toml(&value))?;
        }
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub(crate) fn console_switches(
        paths: &crate::home::paths::PlakPaths,
    ) -> Result<plakd_wire_types::console::ConfiguredSwitches, String> {
        // Startup policy must not enumerate or admit unrelated templates.
        let config = Self::load_config_file(&paths.config_file())?;
        super::web::validate_switches(&config)?;
        Ok(plakd_wire_types::console::ConfiguredSwitches {
            web_accounts_read: config.web_accounts_read,
            web_config_read: config.web_config_read,
            web_credentials_write: config.web_credentials_write,
            web_config_write: config.web_config_write,
        })
    }

    pub(crate) fn from_home_snapshot(
        config: Option<&str>,
        templates: &BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut product = config.map_or_else(|| Ok(Self::default_product()), Self::parse)?;
        for field in product.present.clone() {
            product.origins.insert(field, ConfigValueOrigin::User);
        }
        for (name, text) in templates {
            let template = parse_template(text, name)?;
            product.templates.insert(name.clone(), template);
            product
                .template_origins
                .insert(name.clone(), ConfigValueOrigin::User);
        }
        product.validate()?;
        Ok(product)
    }

    pub(crate) fn console_field(&self, field: ConfigField) -> Option<toml_edit::Value> {
        self.field_value(field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_publication_process_loss_is_old_or_complete_new()
    -> Result<(), Box<dyn std::error::Error>> {
        use super::super::atomic::AtomicStage;
        use std::os::unix::process::ExitStatusExt as _;
        const PHASE: &str = "PLAK_OPERATOR_CRASH_PHASE";
        const DIRECTORY: &str = "PLAK_OPERATOR_CRASH_HOME";
        const KIND: &str = "PLAK_OPERATOR_CRASH_KIND";
        const BODY: &str = "# complete new body\naccount = 'codex'\nmodel = 'gpt-5.6-luna'\n";
        if let Some(phase) = std::env::var_os(PHASE) {
            let paths = crate::home::paths::PlakPaths::explicit(
                std::env::var_os(DIRECTORY).ok_or("child home")?.into(),
            );
            let template = std::env::var(KIND)? == "template";
            let file = if template {
                DefinitionFile::Template("fresh")
            } else {
                DefinitionFile::Config
            };
            let bytes = if template {
                BODY.as_bytes()
            } else {
                b"# complete new config\n"
            };
            let _ = publish_with_fault(&paths, file, bytes, template, &mut |_, stage| {
                if std::ffi::OsStr::new(&format!("{stage:?}")) == phase {
                    let _ = rustix::process::kill_process(
                        rustix::process::getpid(),
                        rustix::process::Signal::KILL,
                    );
                    panic!("crash injection did not terminate child");
                }
                Ok(())
            });
            panic!("child did not reach publication cut");
        }
        for template in [false, true] {
            for phase in [
                AtomicStage::Write,
                AtomicStage::FileSync,
                AtomicStage::Publish,
                AtomicStage::ParentSync,
            ] {
                let (_root, paths) = crate::home::paths::PlakPaths::isolated_test_home();
                let target = if template {
                    paths.user_agents_dir().join("fresh.toml")
                } else {
                    paths.config_file()
                };
                if !template {
                    std::fs::write(&target, "# old config\n")?;
                }
                let output = std::process::Command::new(std::env::current_exe()?)
                    .args(["--exact", "product_config::console::tests::console_publication_process_loss_is_old_or_complete_new", "--nocapture"])
                    .env(PHASE, format!("{phase:?}")).env(DIRECTORY, paths.home()).env(KIND, if template { "template" } else { "config" }).output()?;
                assert_eq!(
                    output.status.signal(),
                    Some(9),
                    "cut {phase:?}, template={template}"
                );
                if phase == AtomicStage::ParentSync {
                    assert_eq!(
                        std::fs::read_to_string(&target)?,
                        if template {
                            BODY
                        } else {
                            "# complete new config\n"
                        }
                    );
                } else if template {
                    assert!(!target.exists());
                } else {
                    assert_eq!(std::fs::read_to_string(&target)?, "# old config\n");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn publication_reports_parent_sync_uncertainty_and_new_name_is_exclusive()
    -> Result<(), Box<dyn std::error::Error>> {
        use super::super::{ConfigSaveError, atomic::AtomicStage};
        use std::os::unix::fs::PermissionsExt as _;
        let (_root, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        std::fs::write(paths.config_file(), "# old\n")?;
        let result = publish_with_fault(
            &paths,
            DefinitionFile::Config,
            b"# complete\n",
            false,
            &mut |_, stage| {
                if stage == AtomicStage::ParentSync {
                    Err("injected parent sync".into())
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(
            result,
            Err(ConfigSaveError::PublishedDurabilityUncertain(_))
        ));
        assert_eq!(
            std::fs::read_to_string(paths.config_file())?,
            "# complete\n"
        );
        assert_eq!(
            std::fs::metadata(paths.config_file())?.permissions().mode() & 0o777,
            0o600
        );
        let target = paths.user_agents_dir().join("fresh.toml");
        let result = publish_with_fault(
            &paths,
            DefinitionFile::Template("fresh"),
            b"# candidate\n",
            true,
            &mut |_, stage| {
                if stage == AtomicStage::Publish {
                    std::fs::write(&target, "# racing creator\n")
                        .map_err(|_| "fixture".to_owned())?;
                }
                Ok(())
            },
        );
        assert!(matches!(result, Err(ConfigSaveError::BeforePublication(_))));
        assert_eq!(std::fs::read_to_string(target)?, "# racing creator\n");
        Ok(())
    }

    #[test]
    fn web_read_switches_are_home_only_false_by_default_and_explicit_on_reload()
    -> Result<(), Box<dyn std::error::Error>> {
        let defaults = ProductConfig::default_product();
        assert!(!defaults.web_accounts_read && !defaults.web_config_read);
        assert!(!defaults.render().contains("[web]"));
        let reference = defaults.render_annotated();
        assert!(reference.contains("# [web]\n"));
        assert!(reference.contains("# accounts_read = false\n"));
        assert!(reference.contains("# config_read = false\n"));
        let config = ProductConfig::parse("[web]\naccounts_read = true\nconfig_read = false\n")?;
        assert!(config.web_accounts_read && !config.web_config_read);
        for name in ["web.accounts_read", "web.config_read"] {
            let key = super::super::config_key(name).ok_or("registered switch")?;
            assert!(key.field.is_home_only());
            assert!(!key.field.is_home_orchestration());
        }
        let home = tempfile::tempdir()?;
        let paths = crate::home::paths::PlakPaths::explicit(home.path().to_path_buf());
        std::fs::write(
            paths.config_file(),
            "[web]\naccounts_read = true\nconfig_read = true\n",
        )?;
        let captured = ProductConfig::console_switches(&paths)?;
        std::fs::write(
            paths.config_file(),
            "[web]\naccounts_read = false\nconfig_read = false\n",
        )?;
        assert!(captured.web_accounts_read && captured.web_config_read);
        let current = ProductConfig::console_switches(&paths)?;
        assert!(!current.web_accounts_read && !current.web_config_read);
        Ok(())
    }
}
