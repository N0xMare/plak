use plakous_hosted::runtime::ResumeAfterLoss;
use plakous_hosted::runtime::operator_attention::AttentionReason;

use super::{PlakPaths, ProductStore, ProductStoreOpenError, views};

pub(crate) struct AttentionRoot {
    pub(crate) session: String,
    pub(crate) tree: String,
    pub(crate) reason: Option<AttentionReason>,
    pub(crate) prepared_wakes: u32,
}

impl ProductStore {
    /// Read current, non-archived roots without opening a session or writing views.
    pub(crate) fn inspect_attention_roots(
        paths: &PlakPaths,
        policy: ResumeAfterLoss,
    ) -> Result<Vec<AttentionRoot>, ProductStoreOpenError> {
        if !paths
            .product_database()
            .try_exists()
            .map_err(|error| ProductStoreOpenError::unavailable(error.to_string()))?
        {
            return Ok(Vec::new());
        }
        Self::read_only_snapshot(paths, |snapshot| {
            let mut statement = snapshot
                .prepare(
                    "SELECT s.id, s.node_tree_id FROM sessions s
                 WHERE s.archived=0 AND s.node_agent_id=s.node_tree_id
                   AND NOT EXISTS (SELECT 1 FROM sessions next WHERE next.source_session_id=s.id
                     AND next.node_tree_id=s.node_tree_id AND next.node_agent_id=s.node_agent_id)
                 ORDER BY s.updated_at DESC,s.id ASC",
                )
                .map_err(|error| error.to_string())?;
            let candidates = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| error.to_string())?;
            let mut verified = views::VerifiedSubmissions::default();
            candidates
                .map(|candidate| {
                    let (session, tree) = candidate.map_err(|error| error.to_string())?;
                    let (reason, prepared_wakes) =
                        views::attention_facts_on(snapshot, &mut verified, &session, policy, true)?;
                    Ok(AttentionRoot {
                        session,
                        tree,
                        reason,
                        prepared_wakes,
                    })
                })
                .collect()
        })
    }
}
