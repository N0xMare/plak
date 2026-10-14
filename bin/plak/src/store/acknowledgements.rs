//! Product-layer acknowledgement of one attention occurrence.
//!
//! The Agents tab keeps a failed, unsubmitted, blocked or recovery-required
//! attempt visible until the operator presses `x`. That press covers one
//! occurrence — the attempt, the attention label and the end stamp — not the
//! attempt for its whole life. A refused recovery keeps the stamp and changes
//! the label; running again and failing takes a new stamp. Neither is covered
//! by the earlier acknowledgement. A `followup` opens a new attempt, which
//! needs none of the old one's acknowledgements.

use super::{ProductStore, from_i64, to_i64};
use crate::surface_protocol::AgentAcknowledgement;

impl ProductStore {
    /// Records one acknowledged occurrence. Repeating it keeps the first stamp.
    pub fn acknowledge_agent_attempt(
        &mut self,
        tree_id: &str,
        acknowledgement: &AgentAcknowledgement,
        acknowledged_at_unix: u64,
    ) -> Result<(), String> {
        #[cfg(feature = "test-hooks")]
        let _settle = super::SettleCommitTrace;
        self.durability(true)?;
        let ended = acknowledgement.ended_at.map(to_i64).transpose()?;
        // `ended_at IS ?` matches NULL to NULL, which a PRIMARY KEY does not
        // (SQLite treats those NULLs as distinct). One statement is atomic.
        self.connection
            .execute(
                "INSERT INTO agent_acknowledgements(tree_id,agent_id,attempt_id,attention,ended_at,acknowledged_at) \
                 SELECT ?1,?2,?3,?4,?5,?6 WHERE NOT EXISTS (\
                 SELECT 1 FROM agent_acknowledgements \
                 WHERE tree_id=?1 AND agent_id=?2 AND attempt_id=?3 AND attention=?4 AND ended_at IS ?5)",
                (
                    tree_id,
                    acknowledgement.agent_id.as_str(),
                    acknowledgement.attempt_id.as_str(),
                    acknowledgement.attention.as_str(),
                    ended,
                    to_i64(acknowledged_at_unix)?,
                ),
            )
            .map_err(|_| "could not record the agent acknowledgement".to_owned())?;
        Ok(())
    }

    /// Every acknowledged occurrence of one tree, ordered for a stable answer.
    pub fn agent_acknowledgements(
        &mut self,
        tree_id: &str,
    ) -> Result<Vec<AgentAcknowledgement>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT agent_id,attempt_id,attention,ended_at FROM agent_acknowledgements \
                 WHERE tree_id=?1 ORDER BY agent_id,attempt_id,attention,ended_at",
            )
            .map_err(|_| "could not read agent acknowledgements".to_owned())?;
        statement
            .query_map([tree_id], |row| {
                let ended: Option<i64> = row.get(3)?;
                Ok(AgentAcknowledgement {
                    agent_id: row.get(0)?,
                    attempt_id: row.get(1)?,
                    attention: row.get(2)?,
                    ended_at: ended.map(from_i64).transpose()?,
                })
            })
            .map_err(|_| "could not read agent acknowledgements".to_owned())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "agent acknowledgement rows are invalid".to_owned())
    }

    /// Deletes every acknowledgement of one tree. Repeating it is success:
    /// session gc runs the deletion again after a partial failure.
    pub fn delete_agent_acknowledgements(&mut self, tree_id: &str) -> Result<(), String> {
        #[cfg(feature = "test-hooks")]
        let _settle = super::SettleCommitTrace;
        self.durability(true)?;
        self.connection
            .execute(
                "DELETE FROM agent_acknowledgements WHERE tree_id=?1",
                [tree_id],
            )
            .map_err(|_| "could not delete agent acknowledgements".to_owned())?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::home::paths::PlakPaths;
    use crate::store::ProductStore;
    use crate::surface_protocol::AgentAcknowledgement;

    fn ack(
        agent: &str,
        attempt: &str,
        attention: &str,
        ended_at: Option<u64>,
    ) -> AgentAcknowledgement {
        AgentAcknowledgement {
            agent_id: agent.to_owned(),
            attempt_id: attempt.to_owned(),
            attention: attention.to_owned(),
            ended_at,
        }
    }

    #[test]
    fn an_acknowledgement_is_one_occurrence_and_survives_reopen() {
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        let recovery = ack("c3", "c3-a1", "needs recovery", Some(10));
        {
            let mut store = ProductStore::open(&paths).unwrap();
            store
                .acknowledge_agent_attempt("tree-1", &recovery, 10)
                .unwrap();
            store
                .acknowledge_agent_attempt("tree-1", &recovery, 20)
                .unwrap();
            store
                .acknowledge_agent_attempt("tree-1", &ack("c3", "c3-a1", "failed", Some(10)), 30)
                .unwrap();
            store
                .acknowledge_agent_attempt("tree-1", &ack("c3", "c3-a1", "failed", None), 40)
                .unwrap();
            store
                .acknowledge_agent_attempt("tree-1", &ack("c3", "c3-a1", "failed", None), 50)
                .unwrap();
            store
                .acknowledge_agent_attempt("tree-2", &recovery, 60)
                .unwrap();
        }
        let mut reopened = ProductStore::open(&paths).unwrap();
        assert_eq!(
            reopened.agent_acknowledgements("tree-1").unwrap(),
            vec![
                ack("c3", "c3-a1", "failed", None),
                ack("c3", "c3-a1", "failed", Some(10)),
                ack("c3", "c3-a1", "needs recovery", Some(10)),
            ],
            "a repeat is one row; a new label or a new end stamp is another occurrence"
        );
        assert_eq!(
            reopened.agent_acknowledgements("tree-3").unwrap(),
            [] as [AgentAcknowledgement; 0]
        );
        reopened.delete_agent_acknowledgements("tree-1").unwrap();
        reopened.delete_agent_acknowledgements("tree-1").unwrap();
        assert_eq!(
            reopened.agent_acknowledgements("tree-1").unwrap(),
            [] as [AgentAcknowledgement; 0]
        );
        assert_eq!(
            reopened.agent_acknowledgements("tree-2").unwrap(),
            vec![recovery],
            "deleting one tree leaves another tree's acknowledgements"
        );
    }
}
