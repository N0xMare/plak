//! Read-only binding liveness metadata without runtime ownership.
use super::{BindingRecordDisposition, PlakPaths, ProductStore, SessionLocator};

impl ProductStore {
    pub(crate) fn inspect_binding_dispositions(
        paths: &PlakPaths,
    ) -> Result<Vec<BindingRecordDisposition>, String> {
        if !paths
            .product_database()
            .try_exists()
            .map_err(|error| error.to_string())?
        {
            return Ok(Vec::new());
        }
        Self::read_only_snapshot(paths, |snapshot| {
            let mut statement = snapshot.prepare("SELECT s.node_tree_id,s.node_agent_id,s.locator,s.archived,EXISTS(SELECT 1 FROM sessions successor WHERE successor.source_session_id=s.id AND successor.node_tree_id=s.node_tree_id AND successor.node_agent_id=s.node_agent_id) FROM sessions s ORDER BY s.id").map_err(|error|error.to_string())?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, bool>(3)?,
                        row.get::<_, bool>(4)?,
                    ))
                })
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            rows.into_iter()
                .map(|(tree, agent, locator, archived, superseded)| {
                    Ok(BindingRecordDisposition {
                        root: tree == agent,
                        tree,
                        locator: locator
                            .map(|locator| {
                                locator
                                    .parse::<SessionLocator>()
                                    .map_err(|error| error.to_string())
                            })
                            .transpose()?,
                        archived,
                        superseded,
                    })
                })
                .collect()
        })
    }
}
