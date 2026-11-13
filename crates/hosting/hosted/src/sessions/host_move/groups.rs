//! Host topology grouping; native runtime stores still operate on complete sessions.
use std::collections::{BTreeMap, BTreeSet};

use plakous::model::SessionId;

/// One indivisible ownership-move group. The id binds its full membership.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MoveGroup {
    id: String,
    trees: BTreeSet<String>,
    sessions: BTreeSet<SessionId>,
}

impl MoveGroup {
    /// Explicit operator identity for Ready and guarded abandonment.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
    /// All trees that must move together, including native line-head dependencies.
    #[must_use]
    pub const fn trees(&self) -> &BTreeSet<String> {
        &self.trees
    }
    /// Complete native sessions, never individual branches of a shared session.
    #[must_use]
    pub const fn sessions(&self) -> &BTreeSet<SessionId> {
        &self.sessions
    }
}

/// Why an exact connected group could not be selected.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupPlanError {
    /// Missing operation/tree identity, or excessive group inputs.
    InvalidInventory,
    /// A current line head belongs outside the carried inventory.
    UnmappedHead(SessionId),
    /// A tree id cannot silently expand an operator's abandonment request.
    ExplicitGroupRequired(String),
    /// No group or tree has that exact identity.
    NotFound,
}

impl std::fmt::Display for GroupPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInventory => {
                f.write_str("invalid or excessive ownership-move group inventory")
            }
            Self::UnmappedHead(session) => write!(
                f,
                "native line-head dependency {session} is outside the carried trees; resolve it with the source build before upgrading"
            ),
            Self::ExplicitGroupRequired(group) => write!(
                f,
                "this tree shares native state; name the explicit group {group} to abandon all of its trees"
            ),
            Self::NotFound => f.write_str("ownership-move group or tree not found"),
        }
    }
}
impl std::error::Error for GroupPlanError {}

/// Disconnected groups remain independently installable and recoverable.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MoveGroups(Vec<MoveGroup>);

impl MoveGroups {
    /// Closes tree membership over shared native sessions and current-head dependencies.
    /// `heads` includes each native session's retired-branch line heads too.
    ///
    /// # Errors
    /// Refuses an invalid inventory or a dependency not represented by carried trees.
    pub fn plan(
        operation: &str,
        trees: &BTreeMap<String, BTreeSet<SessionId>>,
        heads: &BTreeMap<SessionId, BTreeSet<SessionId>>,
    ) -> Result<Self, GroupPlanError> {
        if operation.is_empty()
            || operation.len() > 128
            || trees.is_empty()
            || trees.len() > 4096
            || trees
                .keys()
                .any(|tree| super::super::SessionNode::parse(tree, tree).is_err())
            || trees.values().any(|sessions| sessions.len() > 1024)
        {
            return Err(GroupPlanError::InvalidInventory);
        }
        let mut owners: BTreeMap<SessionId, BTreeSet<&str>> = BTreeMap::new();
        for (tree, sessions) in trees {
            for session in sessions {
                owners.entry(*session).or_default().insert(tree);
            }
        }
        if owners.len() > 16384 {
            return Err(GroupPlanError::InvalidInventory);
        }
        let graph = connected_trees(&owners, heads)?;
        let mut unseen = trees.keys().map(String::as_str).collect::<BTreeSet<_>>();
        let mut groups = Vec::new();
        while let Some(tree) = unseen.pop_first() {
            let mut members = BTreeSet::new();
            let mut sessions = BTreeSet::new();
            let mut pending = vec![tree];
            while let Some(member) = pending.pop() {
                if !members.insert(member.to_owned()) {
                    continue;
                }
                unseen.remove(member);
                sessions.extend(&trees[member]);
                pending.extend(graph.get(member).into_iter().flatten().copied());
            }
            if sessions.len() > 1024 {
                return Err(GroupPlanError::InvalidInventory);
            }
            let names = members.iter().map(String::as_str).collect::<Vec<_>>();
            let native = sessions.iter().map(ToString::to_string).collect::<Vec<_>>();
            let tree_count = names.len().to_string();
            let native_count = native.len().to_string();
            let parts = std::iter::once("host-move/group/v1")
                .chain(std::iter::once(operation))
                .chain([tree_count.as_str(), native_count.as_str()])
                .chain(names)
                .chain(native.iter().map(String::as_str))
                .collect::<Vec<_>>();
            let id = format!("group-{}", super::super::digest_parts(&parts));
            groups.push(MoveGroup {
                id,
                trees: members,
                sessions,
            });
        }
        Ok(Self(groups))
    }

    /// Stable preflight/progress inventory, without silently expanding any action.
    #[must_use]
    pub fn groups(&self) -> &[MoveGroup] {
        &self.0
    }

    /// Selects one explicit group, or a tree only when it is a singleton group.
    ///
    /// # Errors
    /// A multi-tree request names its required explicit group; missing ids refuse.
    pub fn resolve_abandon(&self, target: &str) -> Result<&MoveGroup, GroupPlanError> {
        if let Some(group) = self.0.iter().find(|group| group.id == target) {
            return Ok(group);
        }
        let group = self
            .0
            .iter()
            .find(|group| group.trees.contains(target))
            .ok_or(GroupPlanError::NotFound)?;
        if group.trees.len() > 1 {
            return Err(GroupPlanError::ExplicitGroupRequired(group.id.clone()));
        }
        Ok(group)
    }
}

fn connected_trees<'a>(
    owners: &BTreeMap<SessionId, BTreeSet<&'a str>>,
    heads: &BTreeMap<SessionId, BTreeSet<SessionId>>,
) -> Result<BTreeMap<&'a str, BTreeSet<&'a str>>, GroupPlanError> {
    let mut graph: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (session, trees) in owners {
        let primary = *trees.first().ok_or(GroupPlanError::InvalidInventory)?;
        for tree in trees.iter().skip(1) {
            graph.entry(primary).or_default().insert(tree);
            graph.entry(tree).or_default().insert(primary);
        }
        let dependencies = std::iter::once(session).chain(heads.get(session).into_iter().flatten());
        for dependency in dependencies {
            let linked = owners
                .get(dependency)
                .ok_or(GroupPlanError::UnmappedHead(*dependency))?;
            let sibling = *linked.first().ok_or(GroupPlanError::InvalidInventory)?;
            graph.entry(primary).or_default().insert(sibling);
            graph.entry(sibling).or_default().insert(primary);
        }
    }
    Ok(graph)
}

#[cfg(test)]
mod tests;
