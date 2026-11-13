//! Atomic semantic product seeds, including immutable identity tombstones.
use super::{
    ProductStore, RecordStoreError, bind_identity, commit_in_transaction, to_i64, views,
    write_error,
};
use plakous_hosted::sessions::host_move::{
    DormantSession, DormantSessionDocument, MoveIdentityClaim, MoveIdentityKind,
};
use plakous_hosted::sessions::projector::{
    ProductIntents, ProjectionCommit, SessionViews, ViewWrites,
};
use rusqlite::{OptionalExtension as _, Transaction, TransactionBehavior, params};

#[cfg(test)]
mod tests;

impl ProductStore {
    #[cfg(test)]
    pub(crate) fn move_acknowledgements(
        &self,
        tree: &str,
    ) -> Result<Vec<plakous_hosted::sessions::host_move::MoveAgentAcknowledgement>, String> {
        let mut query = self.connection.prepare("SELECT agent_id,attempt_id,attention,ended_at,acknowledged_at FROM agent_acknowledgements WHERE tree_id=?1 ORDER BY agent_id,attempt_id,attention,ended_at")
            .map_err(|_| "could not read move acknowledgements")?;
        query
            .query_map([tree], |row| {
                Ok(
                    plakous_hosted::sessions::host_move::MoveAgentAcknowledgement {
                        agent_id: row.get(0)?,
                        attempt_id: row.get(1)?,
                        attention: row.get(2)?,
                        ended_at: row
                            .get::<_, Option<i64>>(3)?
                            .map(super::from_i64)
                            .transpose()?,
                        acknowledged_at: super::from_i64(row.get(4)?)?,
                    },
                )
            })
            .map_err(|_| "could not read move acknowledgements")?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "invalid move acknowledgements".into())
    }

    pub(crate) fn move_plan_authority(&self, id: &str) -> Result<Option<String>, String> {
        let authority = self.connection.query_row(
            "SELECT json_extract(seed,'$.plan_authority') FROM session_move_seeds WHERE session_id=?1",
            [id], |row| row.get::<_, Option<String>>(0),
        ).optional().map_err(|_| "could not read move plan-state namespace")?.flatten();
        if authority.as_ref().is_some_and(|value| {
            value.is_empty()
                || value.len() > 59
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        }) {
            return Err("invalid move plan-state namespace".into());
        }
        // This is construction data only. Exact reopen still verifies the frozen
        // executor binding before any tool can use this namespace.
        Ok(authority)
    }

    pub(crate) fn move_identity_claims(
        &self,
        owner: &str,
    ) -> Result<Vec<MoveIdentityClaim>, String> {
        let mut query = self.connection.prepare("SELECT identity,kind,binding_digest,terminal_disposition,terminal_outcome FROM identity_bindings WHERE owner_session_id=?1 ORDER BY identity")
            .map_err(|_| "could not read move identity claims")?;
        query
            .query_map([owner], |row| {
                let kind: String = row.get(1)?;
                Ok(MoveIdentityClaim {
                    identity: row.get(0)?,
                    kind: MoveIdentityKind::parse(&kind).ok_or(rusqlite::Error::InvalidQuery)?,
                    owner: owner.into(),
                    binding: row.get(2)?,
                    disposition: row.get(3)?,
                    outcome: row.get(4)?,
                })
            })
            .map_err(|_| "could not read move identity claims")?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "invalid move identity claims".into())
    }

    pub(crate) fn move_projection_base(&self, id: &str) -> Result<Option<SessionViews>, String> {
        let row: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT seed,seed_digest FROM session_move_seeds WHERE session_id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "could not read semantic move base")?;
        row.map(|(text, digest)| {
            if plakous::model::sha256_hex(&[text.as_bytes()]) != digest {
                return Err("semantic move base digest changed".into());
            }
            let document: DormantSessionDocument =
                serde_json::from_str(&text).map_err(|_| "invalid semantic move base")?;
            if document.id() != id {
                return Err("semantic move base owner changed".into());
            }
            let seed = document.into_cut()?;
            seed.validate_identity_claims()?;
            Ok(seed.views().clone())
        })
        .transpose()
    }

    pub(crate) fn install_move_seed(
        &mut self,
        operation: &str,
        digest: &str,
        seed: &DormantSession,
    ) -> Result<(), RecordStoreError> {
        seed.validate_identity_claims()?;
        let document = seed.document()?;
        let id = document.id().to_owned();
        let text =
            serde_json::to_string(&document).map_err(|_| "could not encode semantic move seed")?;
        let seed_digest = plakous::model::sha256_hex(&[text.as_bytes()]);
        let (mut record, projected, intents) = document.into_cut()?.into_parts();
        self.durability(true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(super::write_refusal)?;
        let previous: Option<(String, String, String)> = tx.query_row(
            "SELECT operation,capsule_digest,seed_digest FROM session_move_seeds WHERE session_id=?1", [&id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional().map_err(|error| write_error(error, "could not inspect move seed"))?;
        if let Some(previous) = previous {
            return if previous == (operation.to_owned(), digest.to_owned(), seed_digest) {
                Ok(())
            } else {
                Err("semantic move seed conflicts with a reserved product identity".into())
            };
        }
        let used: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM identity_bindings WHERE identity=?1)",
                [&record.create_operation_id],
                |row| row.get(0),
            )
            .map_err(|error| write_error(error, "could not inspect move identity"))?;
        if used {
            return Err("semantic move cannot recreate an already used product identity".into());
        }
        record.transcript.clear();
        commit_in_transaction(&tx, &record, 0, None)?;
        for claim in seed.identity_claims() {
            bind_identity(
                &tx,
                claim.kind.as_str(),
                &claim.identity,
                &claim.owner,
                &claim.binding,
                claim.disposition.as_deref(),
                claim.outcome.as_deref(),
            )?;
        }
        write_intents(&tx, &id, &intents)?;
        for acknowledgement in seed.acknowledgements() {
            tx.execute("INSERT INTO agent_acknowledgements(tree_id,agent_id,attempt_id,attention,ended_at,acknowledged_at) SELECT ?1,?2,?3,?4,?5,?6 WHERE NOT EXISTS(SELECT 1 FROM agent_acknowledgements WHERE tree_id=?1 AND agent_id=?2 AND attempt_id=?3 AND attention=?4 AND ended_at IS ?5)",
                params![record.node.tree_id, acknowledgement.agent_id, acknowledgement.attempt_id, acknowledgement.attention, acknowledgement.ended_at.map(to_i64).transpose()?, to_i64(acknowledgement.acknowledged_at)?])
                .map_err(|error| write_error(error, "could not install move acknowledgement"))?;
        }
        let writes = ViewWrites::between(&SessionViews::empty(), &projected);
        if views::commit_views(&tx, &id, &writes, &projected.state)? != ProjectionCommit::Applied {
            return Err("semantic move projection was not installed atomically".into());
        }
        tx.execute(
            "INSERT INTO session_move_seeds VALUES(?1,?2,?3,?4,?5)",
            params![id, operation, digest, seed_digest, text],
        )
        .map_err(|error| write_error(error, "could not persist semantic move base"))?;
        tx.commit()
            .map_err(|error| write_error(error, "could not commit semantic move seed"))?;
        self.baselines.remove(&id);
        Ok(())
    }
}

fn write_intents(
    tx: &Transaction<'_>,
    owner: &str,
    intents: &ProductIntents,
) -> Result<(), RecordStoreError> {
    for submission in &intents.submissions {
        tx.execute("INSERT INTO submissions(session_id,command_id,kind,queue_item_id,queue_revision,target_turn_id,canonical_command,submitted_at_ms,anchor) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![owner, submission.command_id, submission.kind.as_str(), submission.queue_item_id, to_i64(submission.queue_item_revision)?,
                submission.target_turn_id, submission.canonical_command, to_i64(submission.submitted_at_ms)?,
                to_i64(*intents.submission_anchors.get(&submission.command_id).ok_or("missing move submission anchor")?)?])
            .map_err(|error| write_error(error, "could not install move submission"))?;
    }
    for request in &intents.workflow_requests {
        let (reason, selection) = plakous_hosted::sessions::encode_workflow_kind(&request.kind);
        tx.execute("INSERT INTO workflow_requests(session_id,operation_id,source_locator,kind,compact_reason,selection,anchor) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![owner, request.operation_id, request.source_locator.to_string(), request.kind.as_str(), reason, selection,
                to_i64(*intents.workflow_anchors.get(&request.operation_id).ok_or("missing move workflow anchor")?)?])
            .map_err(|error| write_error(error, "could not install move workflow"))?;
    }
    for notice in &intents.notices {
        tx.execute("INSERT INTO operator_notices(session_id,notice_id,anchor,ordinal,row) VALUES(?1,?2,?3,?4,?5)",
            params![owner, notice.notice_id, to_i64(notice.anchor)?, notice.ordinal, serde_json::to_string(&notice.row).map_err(|_| "invalid move notice")?])
            .map_err(|error| write_error(error, "could not install move notice"))?;
    }
    let encode = |kind: &str, anchor, ordinal, value: String| -> Result<(), RecordStoreError> {
        tx.execute(
            "INSERT INTO host_intents VALUES(?1,?2,?3,?4,?5)",
            params![owner, kind, to_i64(anchor)?, ordinal, value],
        )
        .map_err(|error| write_error(error, "could not install move host intent"))?;
        Ok(())
    };
    for intent in &intents.wake_suppressions {
        encode(
            "wake_suppression",
            intent.anchor,
            intent.ordinal,
            "null".into(),
        )?;
    }
    for intent in &intents.pause_causes {
        encode(
            "pause_cause",
            intent.anchor,
            intent.ordinal,
            serde_json::to_string(&intent.value).map_err(|_| "invalid move pause")?,
        )?;
    }
    for intent in &intents.root_interrupts {
        encode(
            "interrupt_cause",
            intent.anchor,
            intent.ordinal,
            intent.value.encode()?,
        )?;
    }
    for intent in &intents.root_continuations {
        encode(
            "continuation_decision",
            intent.anchor,
            intent.ordinal,
            intent.value.encode()?,
        )?;
    }
    Ok(())
}
