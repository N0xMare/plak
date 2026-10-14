//! Plak's root binding: the `[root]` template of `config.toml`.
//!
//! The product's answer to `plakous_hosted`'s `RootBindingDocument`. The
//! detached shell host hands down the six identity rows an operator picked;
//! everything here -- which template `[root] agent` names, minting `default`
//! when it names none, the `[composition]` table, template files, the
//! validate/write/fsync fence -- is product configuration and stays with it.

use crate::home::paths::PlakPaths;
use crate::product_config::{AgentTemplate, ConfigSaveError, ConfigWriteScope, ProductConfig};
use plakous_hosted::runtime::{
    RootBindingDocument, RootBindingDraft, RootBindingEdit, RootBindingSaveError,
};

/// The root binding of one product home.
pub struct ProductRootBindingDocument {
    paths: PlakPaths,
}

impl ProductRootBindingDocument {
    /// The document under `paths`.
    #[must_use]
    pub const fn new(paths: PlakPaths) -> Self {
        Self { paths }
    }

    /// A handle for a `ShellHostConfig`.
    #[must_use]
    pub fn shared(paths: &PlakPaths) -> std::sync::Arc<dyn RootBindingDocument> {
        std::sync::Arc::new(Self::new(paths.clone()))
    }
}

impl RootBindingDocument for ProductRootBindingDocument {
    fn open(&self) -> Result<Box<dyn RootBindingEdit>, String> {
        let config = ProductConfig::load(&self.paths)?;
        Ok(Box::new(OpenProductRootBinding {
            paths: self.paths.clone(),
            config,
        }))
    }
}

struct OpenProductRootBinding {
    paths: PlakPaths,
    config: ProductConfig,
}

fn draft_from(template: &AgentTemplate) -> RootBindingDraft {
    RootBindingDraft {
        account: template.account.clone(),
        model: template.model.clone(),
        effort: template.effort.clone(),
        extra: template.extra.clone(),
        composition: template.composition,
        protocol: template.protocol,
    }
}

/// The identity rows replace the template's; the card body -- description,
/// spawn tier, delegation, `on_mint` and the rest -- is the operator's and is
/// never rewritten by a bind.
fn apply_draft(template: &mut AgentTemplate, draft: RootBindingDraft) {
    template.account = draft.account;
    template.model = draft.model;
    template.effort = draft.effort;
    template.extra = draft.extra;
    template.composition = draft.composition;
    template.protocol = draft.protocol;
}

impl RootBindingEdit for OpenProductRootBinding {
    fn current(&self) -> Option<RootBindingDraft> {
        self.config.root_template().map(draft_from)
    }

    fn composition_for(
        &self,
        account: Option<&str>,
        model: Option<&str>,
    ) -> plakous_hosted::binding::CompositionSelection {
        self.config.composition_for_tuple(account, model)
    }

    fn commit(self: Box<Self>, draft: RootBindingDraft) -> Result<(), RootBindingSaveError> {
        let mut config = self.config;
        // A bind stages a fresh template; config.toml publishes its pointer.
        // A bind with no account or model clears only that pointer.
        if draft.account.is_empty() || draft.model.is_empty() {
            config.clear_root_binding();
        } else {
            apply_draft(config.root_template_mut(), draft);
        }
        // Inside a trusted project the save lands in the project override —
        // the active context owns the binding — never silently flattened
        // into the global file.
        let scope = if config.project.as_ref().is_some_and(|source| source.trusted) {
            ConfigWriteScope::Project
        } else {
            ConfigWriteScope::User
        };
        config
            .save_scoped(&self.paths, scope)
            .map_err(|error| match error {
                ConfigSaveError::BeforePublication(message) => {
                    RootBindingSaveError::BeforePublication(message)
                }
                ConfigSaveError::PublishedDurabilityUncertain(message) => {
                    RootBindingSaveError::PublishedDurabilityUncertain(message)
                }
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "isolated publication fault test")]
mod tests {
    use super::*;

    #[test]
    fn pointer_rename_failure_is_before_publication_and_preserves_active_body() {
        let (_tmp, paths) = PlakPaths::isolated_test_home();
        let mut config = ProductConfig::default_product();
        config.bind_root_tuple("grok", "old-model", None);
        config.save(&paths).unwrap();
        let edit = ProductRootBindingDocument::new(paths.clone())
            .open()
            .unwrap();
        let mut draft = edit.current().unwrap();
        draft.model = "new-model".into();
        // Refuse only the atomic pointer replacement, after the document opened.
        let backup = paths.config_file().with_extension("old");
        std::fs::rename(paths.config_file(), &backup).unwrap();
        std::fs::create_dir(paths.config_file()).unwrap();
        let error = edit.commit(draft).unwrap_err();
        assert!(matches!(error, RootBindingSaveError::BeforePublication(_)));
        std::fs::remove_dir(paths.config_file()).unwrap();
        std::fs::rename(&backup, paths.config_file()).unwrap();
        let actual = ProductConfig::load(&paths).unwrap();
        assert_eq!(actual.default_model(), Some("old-model"));
        assert!(paths.user_agents_dir().join("default-2.toml").is_file());
    }
}
