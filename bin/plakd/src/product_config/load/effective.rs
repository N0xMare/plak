//! Read configuration files and combine effective scopes.

use super::{
    ConfigField, ConfigLoadError, ConfigValueOrigin, ProductConfig, ProjectConfigSource,
    optional_input,
};
use crate::home::paths::PlakPaths;
use crate::product_config::{project_trust, templates::load_templates_dir};
use std::fs;
use std::path::{Path, PathBuf};

fn checked_project_root(paths: &PlakPaths, cacheable: &mut bool) -> Option<(PathBuf, bool)> {
    let root = paths.project_root()?;
    let Ok(canonical) = fs::canonicalize(root) else {
        *cacheable = false;
        return Some((root.to_path_buf(), false));
    };
    let trusted = match project_trust::is_canonical_trusted(paths, &canonical) {
        Ok(trusted) => trusted,
        Err(error) => {
            *cacheable &= !matches!(error, ConfigLoadError::Io(_));
            false
        }
    };
    Some((canonical, trusted))
}

impl ProductConfig {
    /// The effective configuration: `config.toml` plus `~/.plak/agents`, and
    /// — when a trusted project `.plak` is discovered — that project's
    /// `config.toml` key overlay and `agents/` whole-file template
    /// replacements. An absent `config.toml` is the schema default; an absent
    /// agents directory is an empty template set.
    ///
    /// An *untrusted* project applies nothing: the load still succeeds on the
    /// global configuration, `project` names the blocked source and
    /// `warnings` carries the sentence the surfaces show.
    pub fn load(paths: &PlakPaths) -> Result<Self, String> {
        Self::load_effective(paths, &mut true).map_err(String::from)
    }

    pub(crate) fn reload_policy(paths: &PlakPaths) -> (bool, bool) {
        let mut cacheable = true;
        let config = Self::load_effective(paths, &mut cacheable);
        cacheable &= !matches!(config, Err(ConfigLoadError::Io(_)));
        (config.is_ok_and(|config| config.reload_requests), cacheable)
    }

    fn load_effective(paths: &PlakPaths, cacheable: &mut bool) -> Result<Self, ConfigLoadError> {
        let mut config = Self::load_user_classified(paths, cacheable)?;
        config.merge_project(paths, cacheable)?;
        Ok(config)
    }

    /// The global configuration alone, with no project overlay. Every
    /// reference a global file makes must resolve globally — a project
    /// template cannot satisfy a global `root.agent` or `working_set` name —
    /// and save paths diff against this baseline so a write in one scope
    /// cannot flatten the other's values.
    pub fn load_user(paths: &PlakPaths) -> Result<Self, String> {
        Self::load_user_classified(paths, &mut true).map_err(String::from)
    }

    fn load_user_classified(
        paths: &PlakPaths,
        cacheable: &mut bool,
    ) -> Result<Self, ConfigLoadError> {
        let mut config = Self::read_config_file(&paths.config_file()).map_err(|error| {
            error.context(|message| format!("{message}; run `plakd config edit` to fix it"))
        })?;
        for field in config.present.clone() {
            config.origins.insert(field, ConfigValueOrigin::User);
        }
        let (templates, origins) =
            load_templates_dir(&paths.user_agents_dir(), ConfigValueOrigin::User, cacheable)?;
        config.templates = templates;
        config.template_origins = origins;
        config.validate().map_err(ConfigLoadError::Invalid)?;
        Ok(config)
    }

    /// The home's `config.toml` alone: no templates, no project overlay. A
    /// read or parse failure is an error, never the default product.
    pub(crate) fn load_home_file(paths: &PlakPaths) -> Result<Self, String> {
        Self::load_config_file(&paths.config_file())
    }

    pub(in crate::product_config) fn load_config_file(path: &Path) -> Result<Self, String> {
        Self::read_config_file(path).map_err(String::from)
    }

    fn read_config_file(path: &Path) -> Result<Self, ConfigLoadError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default_product());
            }
            Err(error) => {
                return Err(ConfigLoadError::Io(format!(
                    "could not read {}: {error}",
                    path.display()
                )));
            }
        };
        // A load error names the file it came from, exactly as a template
        // load error does.
        Self::parse(&text)
            .map_err(|error| ConfigLoadError::Invalid(format!("{}: {error}", path.display())))
    }

    /// The trusted-project half of `load`: nothing when there is no project
    /// home, nothing but the named warning when the project is untrusted, and
    /// the key overlay plus whole-template replacements when it is.
    fn merge_project(
        &mut self,
        paths: &PlakPaths,
        cacheable: &mut bool,
    ) -> Result<(), ConfigLoadError> {
        let Some(home) = paths.project_home() else {
            return Ok(());
        };
        // Launching inside the product home's parent would otherwise read
        // `~/.plak` as its own "project" override. Both sides canonicalize so
        // an aliased PLAK_HOME still answers true.
        if home == paths.home()
            || optional_input(fs::canonicalize(&home), cacheable).as_deref()
                == optional_input(fs::canonicalize(paths.home()), cacheable).as_deref()
        {
            return Ok(());
        }
        let config_path = home.join("config.toml");
        let agents_dir = home.join("agents");
        let has_config = optional_input(fs::metadata(&config_path), cacheable)
            .is_some_and(|metadata| metadata.is_file());
        let has_agents = optional_input(fs::metadata(&agents_dir), cacheable)
            .is_some_and(|metadata| metadata.is_dir());
        if !has_config && !has_agents {
            return Ok(());
        }
        let Some((root, trusted)) = checked_project_root(paths, cacheable) else {
            return Ok(());
        };
        if !trusted {
            let mut sources = Vec::new();
            if has_config {
                sources.push("config.toml");
            }
            if has_agents {
                sources.push("agents/");
            }
            self.project = Some(ProjectConfigSource {
                root,
                trusted: false,
            });
            self.warnings.push(format!(
                "project {} is not trusted; its {} not applied (run `plakd config trust` to enable it)",
                home.display(),
                sources.join(" and ")
            ));
            return Ok(());
        }
        if has_config {
            let overlay = Self::read_config_file(&config_path).map_err(|error| {
                error.context(|message| format!("{message}; edit this file to fix it"))
            })?;
            if overlay
                .present
                .iter()
                .any(|field| field.is_home_orchestration())
            {
                self.warnings.push(format!(
                    "{}: [orchestration] is home-scoped and ignored here; set it in {}",
                    config_path.display(),
                    paths.config_file().display(),
                ));
            }
            if overlay
                .present
                .contains(&ConfigField::McpClientIdMetadataUrl)
            {
                self.warnings.push(format!(
                    "{}: [mcp] client_id_metadata_url is home-scoped and ignored here; set it in {}",
                    config_path.display(),
                    paths.config_file().display(),
                ));
            }
            self.warn_forward_home_scoped(&overlay, &config_path, paths);
            for field in overlay.present.clone() {
                if field.is_home_only() {
                    continue;
                }
                self.assign_overlay_field(field, &overlay);
                self.origins.insert(field, ConfigValueOrigin::Project);
            }
            self.tool_sections.overlay(&overlay.tool_sections);
        }
        if has_agents {
            let (templates, origins) =
                load_templates_dir(&agents_dir, ConfigValueOrigin::Project, cacheable)?;
            for (name, template) in templates {
                self.templates.insert(name, template);
            }
            self.template_origins.extend(origins);
        }
        self.project = Some(ProjectConfigSource {
            root,
            trusted: true,
        });
        // The global half already validated; a merged failure is something
        // the project asked for, so the error names its document.
        self.validate().map_err(|error| {
            let source = if has_config {
                &config_path
            } else {
                &agents_dir
            };
            ConfigLoadError::Invalid(format!("{}: {error}", source.display()))
        })
    }
}
