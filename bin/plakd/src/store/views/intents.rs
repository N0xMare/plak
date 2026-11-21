use std::collections::HashSet;

use plakous::sessions::SessionLocator;
use plakous_hosted::runtime::ResumeAfterLoss;
use plakous_hosted::runtime::operator_attention::{AttentionReason, root_reason};
use plakous_hosted::session_store::WakeControls;
use plakous_hosted::sessions::projector::{
    AnchoredIntent, OperatorNotice, ProductIntents, RootContinuation, RootInterrupt, SessionViews,
    WorkflowRequest,
};
use rusqlite::Connection;

use super::super::decode_workflow_kind;
use super::{
    PAUSE_CAUSE, ProductStore, ROOT_CONTINUATION, ROOT_INTERRUPT, VerifiedSubmissions,
    WAKE_SUPPRESSION, from_i64, intent_anchors, invalid, load_submissions_where, unavailable,
    view_cursor_and_state,
};

impl ProductStore {
    /// Every intent of one session the projector reads.
    pub fn load_intents(&mut self, session_id: &str) -> Result<ProductIntents, String> {
        intents_of(&self.connection, &mut self.verified_submissions, session_id)
    }

    pub(crate) fn load_intents_without_notices(
        &mut self,
        session_id: &str,
    ) -> Result<ProductIntents, String> {
        read_intents(
            &self.connection,
            &mut self.verified_submissions,
            session_id,
            false,
        )
    }

    pub(crate) fn load_notice_ids(&self, session_id: &str) -> Result<HashSet<String>, String> {
        self.connection
            .prepare("SELECT notice_id FROM operator_notices WHERE session_id=?1")
            .map_err(unavailable)?
            .query_map([session_id], |row| row.get(0))
            .map_err(unavailable)?
            .collect::<Result<_, _>>()
            .map_err(invalid)
    }

    pub(crate) fn load_wake_controls(&self, session_id: &str) -> Result<WakeControls, String> {
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(unavailable)?;
        let mut views = SessionViews::empty();
        if let Some((_, state)) = view_cursor_and_state(&tx, session_id)? {
            views.state = state;
        }
        let mut intents = ProductIntents::default();
        read_host_intents(
            &tx,
            "SELECT kind,anchor,ordinal,value FROM host_intents WHERE session_id=?1 AND kind IN ('wake_suppression','pause_cause') ORDER BY anchor,ordinal",
            session_id,
            &mut intents,
        )?;
        if intents
            .pause_causes
            .iter()
            .any(|pause| pause.value.root_preparation_refusal_command().is_some())
            || (views.pause_cause(&intents).is_none()
                && views.state.last_root_turn.as_ref().is_some_and(|turn| {
                    plakous_hosted::runtime::is_root_loss_command(&turn.command_id)
                }))
        {
            read_host_intents(
                &tx,
                "SELECT kind,anchor,ordinal,value FROM host_intents WHERE session_id=?1 AND kind IN ('interrupt_cause','continuation_decision') ORDER BY anchor,ordinal",
                session_id,
                &mut intents,
            )?;
        }
        tx.commit().map_err(unavailable)?;
        Ok(WakeControls::from_join(&views, &intents))
    }

    pub(crate) fn load_attention_reason(
        &mut self,
        session_id: &str,
        policy: ResumeAfterLoss,
    ) -> Result<Option<AttentionReason>, String> {
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(unavailable)?;
        attention_facts_on(
            &tx,
            &mut self.verified_submissions,
            session_id,
            policy,
            false,
        )
        .map(|(reason, _)| reason)
    }
}

pub(in crate::store) fn attention_facts_on(
    connection: &Connection,
    verified: &mut VerifiedSubmissions,
    session_id: &str,
    policy: ResumeAfterLoss,
    include_prepared_wakes: bool,
) -> Result<(Option<AttentionReason>, u32), String> {
    let mut views = SessionViews::empty();
    if let Some((_, state)) = view_cursor_and_state(connection, session_id)? {
        views.state = state;
    }
    let mut intents = ProductIntents::default();
    read_host_intents(
        connection,
        "SELECT kind,anchor,ordinal,value FROM host_intents WHERE session_id=?1 AND kind IN ('pause_cause','interrupt_cause','continuation_decision') ORDER BY anchor,ordinal",
        session_id,
        &mut intents,
    )?;
    if intents
        .root_interrupts
        .iter()
        .max_by_key(|intent| (intent.anchor, intent.ordinal))
        .is_some_and(|intent| {
            intent.value.cause == plakous_hosted::sessions::projector::RootInterruptCause::OwnerLost
        })
    {
        intents.submission_anchors =
            intent_anchors(connection, session_id, "submissions", "command_id")?;
        intents.submissions =
            load_submissions_where(connection, verified, "session_id=?1", session_id)?
                .into_iter()
                .map(|(_, submission)| submission)
                .collect();
    }
    let reason = root_reason(&views, &intents, policy);
    let prepared_wakes = if include_prepared_wakes && reason.is_some() {
        pending_wakes(connection, session_id, &views)?
    } else {
        0
    };
    Ok((reason, prepared_wakes))
}

fn pending_wakes(connection: &Connection, id: &str, views: &SessionViews) -> Result<u32, String> {
    let mut statement = connection
        .prepare(
            "SELECT command_id,anchor FROM submissions WHERE session_id=?1 AND kind='host_wake'",
        )
        .map_err(unavailable)?;
    let rows = statement
        .query_map([id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(unavailable)?;
    let mut count = 0_u32;
    for row in rows {
        let (id, anchor) = row.map_err(invalid)?;
        let anchor = from_i64(anchor).map_err(invalid)?;
        if !plakous_hosted::runtime::is_root_loss_command(&id)
            && views
                .state
                .retained_from
                .is_none_or(|floor| anchor >= floor)
            && !views.state.deliveries.contains_key(&id)
        {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

pub(in crate::store) fn intents_of(
    connection: &Connection,
    verified: &mut VerifiedSubmissions,
    session_id: &str,
) -> Result<ProductIntents, String> {
    read_intents(connection, verified, session_id, true)
}

pub(in crate::store) fn read_intents(
    connection: &Connection,
    verified: &mut VerifiedSubmissions,
    session_id: &str,
    with_notices: bool,
) -> Result<ProductIntents, String> {
    let submissions = load_submissions_where(connection, verified, "session_id=?1", session_id)?
        .into_iter()
        .map(|(_, submission)| submission)
        .collect();
    let mut statement = connection
        .prepare("SELECT operation_id,source_locator,kind,compact_reason,selection FROM workflow_requests WHERE session_id=?1 ORDER BY rowid")
        .map_err(unavailable)?;
    let workflow_requests = statement
        .query_map([session_id], |row| {
            let locator: String = row.get(1)?;
            Ok(WorkflowRequest {
                operation_id: row.get(0)?,
                source_locator: locator
                    .parse::<SessionLocator>()
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                kind: decode_workflow_kind(&row.get::<_, String>(2)?, row.get(3)?, row.get(4)?)?,
            })
        })
        .map_err(unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)?;
    drop(statement);
    let mut intents = ProductIntents {
        submission_anchors: intent_anchors(connection, session_id, "submissions", "command_id")?,
        workflow_anchors: intent_anchors(
            connection,
            session_id,
            "workflow_requests",
            "operation_id",
        )?,
        submissions,
        workflow_requests,
        notices: if with_notices {
            read_notices(connection, session_id)?
        } else {
            Vec::new()
        },
        ..ProductIntents::default()
    };
    read_host_intents(
        connection,
        "SELECT kind,anchor,ordinal,value FROM host_intents WHERE session_id=?1 ORDER BY anchor,ordinal",
        session_id,
        &mut intents,
    )?;
    Ok(intents)
}

fn read_notices(connection: &Connection, session_id: &str) -> Result<Vec<OperatorNotice>, String> {
    connection
        .prepare("SELECT notice_id,anchor,ordinal,row FROM operator_notices WHERE session_id=?1 ORDER BY anchor,ordinal")
        .map_err(unavailable)?
        .query_map([session_id], |row| {
            let text: String = row.get(3)?;
            Ok(OperatorNotice {
                notice_id: row.get(0)?,
                anchor: from_i64(row.get(1)?)?,
                ordinal: row.get(2)?,
                row: serde_json::from_str(&text).map_err(|_| rusqlite::Error::InvalidQuery)?,
            })
        })
        .map_err(unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)
}

fn read_host_intents(
    connection: &Connection,
    query: &str,
    session_id: &str,
    intents: &mut ProductIntents,
) -> Result<(), String> {
    let mut statement = connection.prepare(query).map_err(unavailable)?;
    let rows = statement
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                from_i64(row.get(1)?)?,
                row.get::<_, u32>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)?;
    for (kind, anchor, ordinal, value) in rows {
        match kind.as_str() {
            WAKE_SUPPRESSION => intents.wake_suppressions.push(AnchoredIntent {
                anchor,
                ordinal,
                value: (),
            }),
            PAUSE_CAUSE => intents.pause_causes.push(AnchoredIntent {
                anchor,
                ordinal,
                value: serde_json::from_str(&value)
                    .map_err(|_| "pause cause intent is invalid".to_owned())?,
            }),
            ROOT_INTERRUPT => intents.root_interrupts.push(AnchoredIntent {
                anchor,
                ordinal,
                value: RootInterrupt::decode(&value)?,
            }),
            ROOT_CONTINUATION => intents.root_continuations.push(AnchoredIntent {
                anchor,
                ordinal,
                value: RootContinuation::decode(&value)?,
            }),
            _ => return Err("host intent is invalid".to_owned()),
        }
    }
    Ok(())
}
