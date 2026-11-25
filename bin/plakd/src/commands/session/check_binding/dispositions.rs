//! Product archival and lineage metadata; no tree reopen or custody.

use super::{CliFailure, CliResult, PlakPaths};
use plakous::sessions::SessionLocator;
use std::collections::{HashMap, HashSet};

pub(super) struct Dispositions {
    pub(super) rows: HashMap<String, (bool, bool)>,
    pub(super) children_closed: HashMap<String, bool>,
}

pub(super) fn key(locator: SessionLocator) -> String {
    format!("{}/{}", locator.session_id(), locator.branch_id())
}

pub(super) fn inspect(paths: &PlakPaths) -> CliResult<Dispositions> {
    let records =
        crate::store::ProductStore::inspect_binding_dispositions(paths).map_err(CliFailure::new)?;
    let archived_trees = records
        .iter()
        .filter(|record| record.root && record.archived && !record.superseded)
        .map(|record| record.tree.clone())
        .collect::<HashSet<_>>();
    let mut rows = HashMap::new();
    for record in records {
        if let Some(locator) = record.locator {
            rows.insert(key(locator), (record.archived, record.superseded));
        }
    }
    let nodes = plakous_recursive_subagents_store::SqliteTreeStore::inspect_node_locators(
        &paths.subagents_database(),
    )
    .map_err(|error| CliFailure::new(error.message()))?;
    let mut children_closed = HashMap::new();
    for (tree, locator, child_closed) in nodes {
        let tree_archived = archived_trees.contains(&tree);
        if tree_archived || child_closed.is_some() {
            let Ok(locator) = locator.parse::<SessionLocator>() else {
                continue;
            };
            let key = key(locator);
            if let Some(closed) = child_closed {
                children_closed.insert(key.clone(), closed);
            }
            if tree_archived {
                rows.entry(key).or_insert((true, false)).0 = true;
            }
        }
    }
    Ok(Dispositions {
        rows,
        children_closed,
    })
}
