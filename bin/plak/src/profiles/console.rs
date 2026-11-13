//! Profile projection from already bounded bytes, never another filesystem read.

use super::{
    AgentTemplate, ProductConfig, ProfileRow, Resolved, TemplateName, UserFile, builtins,
    resolve_builtin, resolve_profile_file, row,
};
use plakd_wire_types::console::{ValidationCode, ValidationIssue};
use std::collections::BTreeMap;

pub(crate) fn builtin_id(id: &str) -> bool {
    builtins().any(|builtin| builtin.id == id)
}

pub(crate) fn validate_user(
    config: &ProductConfig,
    id: &str,
    text: &str,
) -> Result<(), ValidationIssue> {
    let issue = |field: &str, code, detail: &'static str| ValidationIssue {
        field: field.to_owned(),
        code,
        detail: detail.to_owned(),
    };
    let file: UserFile = toml::from_str(text).map_err(|_| {
        issue(
            "body",
            ValidationCode::InvalidValue,
            "the profile must contain a root and a list of template names",
        )
    })?;
    if file.root.is_empty() {
        return Err(issue(
            "root",
            ValidationCode::Empty,
            "a root template is required",
        ));
    }
    if file.working_set.is_empty() {
        return Err(issue(
            "working_set",
            ValidationCode::Empty,
            "working_set must contain at least one template",
        ));
    }
    plakous_hosted::agent_tree_protocol::WorkingSet::parse(file.working_set.clone()).map_err(
        |_| {
            issue(
                "working_set",
                ValidationCode::InvalidValue,
                "working_set must contain valid template names",
            )
        },
    )?;
    if !config.templates.contains_key(&file.root) {
        return Err(issue(
            "root",
            ValidationCode::UnknownTemplate,
            "the root template does not exist",
        ));
    }
    for (index, name) in file.working_set.iter().enumerate() {
        if !config.templates.contains_key(name) {
            return Err(issue(
                &format!("working_set[{index}]"),
                ValidationCode::UnknownTemplate,
                "the working_set template does not exist",
            ));
        }
    }
    let resolved = resolve_profile_file(config, id, file).map_err(|_| {
        issue(
            "body",
            ValidationCode::InvalidValue,
            "the profile cannot be resolved",
        )
    })?;
    super::prepare_resolved(config, resolved)
        .map(|_| ())
        .map_err(|_| {
            issue(
                "root",
                ValidationCode::InvalidValue,
                "the profile is incompatible with the current configuration",
            )
        })
}

pub(crate) fn hash_builtins(hash: &mut impl sha2::Digest) {
    for builtin in builtins() {
        hash.update(builtin.id.as_bytes());
        for file in builtin.files {
            hash.update(file.body.as_bytes());
        }
    }
}

pub(crate) struct ReadProfile {
    pub row: ProfileRow,
    pub root: String,
    pub working_set: Vec<String>,
    pub builtin: bool,
}

pub(crate) struct ReadCatalog {
    pub profiles: Vec<ReadProfile>,
    pub current: Option<String>,
    /// Logical ids only; parser prose can carry private content.
    pub unavailable: Vec<String>,
    pub builtin_templates: BTreeMap<String, AgentTemplate>,
}

pub(crate) fn catalog(
    config: &ProductConfig,
    files: &BTreeMap<String, String>,
    mut admit: impl FnMut(&ProfileRow) -> bool,
) -> Result<ReadCatalog, ()> {
    let mut profiles = Vec::new();
    let mut unavailable = Vec::new();
    let mut builtin_templates = BTreeMap::new();
    for builtin in builtins() {
        match resolve_builtin(config, builtin) {
            Ok(resolved) => {
                for (name, template) in std::iter::once(&resolved.root).chain(&resolved.cards) {
                    builtin_templates
                        .entry(name.clone())
                        .or_insert_with(|| template.clone());
                }
                let profile = project(config, &resolved, true);
                if !admit(&profile.row) {
                    return Err(());
                }
                profiles.push(profile);
            }
            Err(_) => unavailable.push(builtin.id.to_owned()),
        }
    }
    for (id, text) in files {
        if builtins().any(|builtin| builtin.id == id) {
            unavailable.push(id.clone());
            continue;
        }
        match resolve_user(config, id, text) {
            Ok(resolved) => {
                let profile = project(config, &resolved, false);
                if !admit(&profile.row) {
                    return Err(());
                }
                profiles.push(profile);
            }
            Err(_) => unavailable.push(id.clone()),
        }
    }
    let root = config.root_agent.as_deref();
    let current = profiles
        .iter()
        .find(|profile| {
            Some(profile.root.as_str()) == root
                && profile.working_set.iter().map(String::as_str).eq(config
                    .working_set
                    .names()
                    .iter()
                    .map(TemplateName::as_str))
        })
        .map(|profile| profile.row.id.clone());
    Ok(ReadCatalog {
        profiles,
        current,
        unavailable,
        builtin_templates,
    })
}

fn project(config: &ProductConfig, resolved: &Resolved, builtin: bool) -> ReadProfile {
    ReadProfile {
        row: row(config, resolved, Vec::new()),
        root: resolved.root.0.clone(),
        working_set: resolved
            .cards
            .iter()
            .map(|(name, _)| name.clone())
            .collect(),
        builtin,
    }
}

pub(super) fn resolve_user(
    config: &ProductConfig,
    id: &str,
    text: &str,
) -> Result<Resolved, String> {
    let file: UserFile = toml::from_str(text).map_err(|error| error.message().to_owned())?;
    // A listed profile must be selectable under the existing working-set
    // grammar before any repeated template bodies are cloned.
    plakous_hosted::agent_tree_protocol::WorkingSet::parse(file.working_set.clone())?;
    resolve_profile_file(config, id, file)
}
