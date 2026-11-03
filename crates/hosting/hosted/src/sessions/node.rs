//! The product record's node identity and the copies it keeps of what the
//! node runs as and where (spec 02 §2.6): the node, the profile of a virtual
//! tree of one, and the display copy of its runner and workspace.
//!
//! Each is encoded and decoded only here, refuses anything this build did not
//! write, and is validated before every store write.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent_tree_protocol::TemplateName;

/// The agent node a session is: its tree and its agent in that tree. Written
/// once, when the record is created, and never rewritten.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionNode {
    /// The tree the node belongs to.
    pub tree_id: String,
    /// The node's agent id in that tree.
    pub agent_id: String,
}

impl SessionNode {
    /// The root of a new tree named after `session_id`: the node every
    /// session that starts a tree of its own is. A genesis tree names its
    /// root agent after the tree, as the tree store's bootstrap does.
    #[must_use]
    pub fn genesis(session_id: &str) -> Self {
        Self {
            tree_id: session_id.to_owned(),
            agent_id: session_id.to_owned(),
        }
    }

    /// Admits a stored node.
    ///
    /// # Errors
    /// Refuses an id outside `[A-Za-z0-9_-]{1,128}`.
    pub fn parse(tree_id: &str, agent_id: &str) -> Result<Self, String> {
        let node = Self {
            tree_id: tree_id.to_owned(),
            agent_id: agent_id.to_owned(),
        };
        node.validate()?;
        Ok(node)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if !is_node_id(&self.tree_id) || !is_node_id(&self.agent_id) {
            return Err("session node is invalid".to_owned());
        }
        Ok(())
    }
}

fn is_node_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// The profile of a session's root while its tree has no rows (a virtual
/// tree of one): the root template and the working set it spawns from. A tree
/// with rows records its profile in its own epochs instead.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RootProfile {
    /// The root template, when the root runs one.
    pub root_template: Option<String>,
    /// The templates the root may spawn, in order.
    pub working_set: Vec<String>,
}

impl RootProfile {
    /// The profile a tree genesis freezes from `roster`: its root template
    /// and its working set.
    #[must_use]
    pub fn from_roster(roster: &crate::orchestration::RosterInput) -> Self {
        Self {
            root_template: roster.root.template.clone(),
            working_set: roster
                .working_set
                .iter()
                .map(|name| name.as_str().to_owned())
                .collect(),
        }
    }

    /// The stored encoding.
    ///
    /// # Errors
    /// Returns a message for a profile that does not validate.
    pub fn encode(&self) -> Result<String, String> {
        self.validate()?;
        serde_json::to_string(self).map_err(|_| "root profile is not serializable".to_owned())
    }

    /// Decodes [`Self::encode`]'s JSON.
    ///
    /// # Errors
    /// Returns a message for anything this build did not write.
    pub fn decode(text: &str) -> Result<Self, String> {
        let profile: Self =
            serde_json::from_str(text).map_err(|_| "root profile is corrupt".to_owned())?;
        profile.validate()?;
        Ok(profile)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        for name in self.root_template.iter().chain(&self.working_set) {
            TemplateName::parse(name).map_err(|_| "root profile names an invalid template")?;
        }
        if !self.working_set.iter().all(|name| seen.insert(name)) {
            return Err("root profile lists a template twice".to_owned());
        }
        Ok(())
    }
}

/// A display copy of the runner a session's root runs on and its workspace
/// there, as the binding named them when the record was written. `None` on a
/// record is the host's own local runner at the record's `workspace_root`,
/// which is every session today. Never authority: the binding is.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerDisplay {
    /// The runner's configured name.
    pub name: String,
    /// The runner's stable identity.
    pub runner_id: String,
    /// The workspace root the runner registered.
    pub workspace_root: PathBuf,
    /// The root's working directory, inside `workspace_root`.
    pub workspace_cwd: PathBuf,
}

impl RunnerDisplay {
    /// The stored encoding.
    ///
    /// # Errors
    /// Returns a message for a copy that does not validate.
    pub fn encode(&self) -> Result<String, String> {
        self.validate()?;
        serde_json::to_string(self).map_err(|_| "runner display is not serializable".to_owned())
    }

    /// Decodes [`Self::encode`]'s JSON.
    ///
    /// # Errors
    /// Returns a message for anything this build did not write.
    pub fn decode(text: &str) -> Result<Self, String> {
        let display: Self =
            serde_json::from_str(text).map_err(|_| "runner display is corrupt".to_owned())?;
        display.validate()?;
        Ok(display)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        let visible = |value: &str, max: usize| {
            (1..=max).contains(&value.len())
                && !value
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        };
        if !visible(&self.name, 64)
            || !visible(&self.runner_id, 128)
            || !self.workspace_root.is_absolute()
            || !self.workspace_cwd.starts_with(&self.workspace_root)
        {
            return Err("runner display is invalid".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
