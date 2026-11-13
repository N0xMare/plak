//! Product archival and lineage metadata; no tree reopen or custody.

use super::{CliFailure, CliResult, PlakPaths};
use plakous::sessions::SessionLocator;
use std::collections::{HashMap, HashSet};

pub(super) struct Dispositions {
    pub(super) rows: HashMap<String, (bool, bool)>,
    pub(super) excluded: Vec<SessionLocator>,
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
    let mut excluded = Vec::new();
    for record in records {
        if let Some(locator) = record.locator {
            rows.insert(key(locator), (record.archived, record.superseded));
            if record.archived || record.superseded {
                excluded.push(locator);
            }
        }
    }
    if !archived_trees.is_empty() {
        let nodes = plakous_recursive_subagents_store::SqliteTreeStore::inspect_node_locators(
            &paths.subagents_database(),
        )
        .map_err(|error| CliFailure::new(error.message()))?;
        for (tree, locator) in nodes {
            if archived_trees.contains(&tree) {
                let locator = locator
                    .parse::<SessionLocator>()
                    .map_err(|error| CliFailure::new(error.to_string()))?;
                rows.entry(key(locator)).or_insert((true, false)).0 = true;
                excluded.push(locator);
            }
        }
    }
    Ok(Dispositions { rows, excluded })
}
