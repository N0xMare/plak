//! Child commands as `agent_commands` intents (spec 03 §14.1): every child
//! Start, wake, mail Start, followup and corrective round is written to the
//! tree with its exact text and message batch before it is submitted. What
//! became of it is the child feed's `CommandAccepted` or `CommandRejected`
//! fact, which only the tree projector applies. The submission site validates
//! that view, records below-retention consumption, or withdraws a command the
//! runtime definitely refused before admitting it.

#[cfg(test)]
mod tests;

use crate::agent_tree::{AgentCommand, AgentCommandKind, MailDurableState};
use crate::agent_tree_protocol::AttemptId;
use crate::agent_tree_tools::TreeDriver;

use crate::driver::ledger::{
    BatchRule, CommandIntent, CommandKind, CommandLedger, LedgerError, LedgerFuture, LedgerStep,
    Prepared, Settlement,
};

impl Prepared {
    fn of(command: &AgentCommand) -> Self {
        Self {
            prompt: command.text.clone(),
            message_ids: command.message_ids.clone(),
            instruction: matches!(
                command.kind,
                AgentCommandKind::Wake | AgentCommandKind::Corrective | AgentCommandKind::Resume
            ),
        }
    }
}

pub(super) struct ChildLedger<'a> {
    driver: &'a TreeDriver,
    recipient: Option<&'a str>,
}

impl<'a> ChildLedger<'a> {
    pub(super) fn new(driver: &'a TreeDriver, recipient: &'a str) -> Self {
        Self {
            driver,
            recipient: Some(recipient),
        }
    }

    pub(super) fn settlement_only(driver: &'a TreeDriver) -> Self {
        Self {
            driver,
            recipient: None,
        }
    }
}

impl CommandLedger for ChildLedger<'_> {
    fn prepare<'a>(&'a mut self, intent: CommandIntent<'a>) -> LedgerStep<'a, Prepared> {
        Box::pin(async move {
            let recipient = self.recipient.ok_or_else(|| {
                LedgerError::Integrity("child preparation requires a recipient".into())
            })?;
            prepare(self.driver, recipient, intent).await
        })
    }

    fn settle<'a>(&'a mut self, command_id: &'a str, settlement: Settlement) -> LedgerStep<'a, ()> {
        Box::pin(async move {
            if matches!(settlement, Settlement::Refused { pause: Some(_) }) {
                return Err(LedgerError::Integrity(
                    "child settlement cannot pause the product queue".into(),
                ));
            }
            acknowledge(self.driver, command_id, settlement).await
        })
    }

    fn duplicate_consumed<'a>(
        &'a self,
        sessions: &'a plakous::sessions::AgentSessions,
        session: plakous::model::SessionId,
        command_id: &'a str,
    ) -> LedgerFuture<'a> {
        Box::pin(
            async move { reconcile_duplicate(self.driver, sessions, session, command_id).await },
        )
    }
}

async fn persist_context<T: Send + 'static>(
    driver: &TreeDriver,
    work: impl FnOnce(&mut crate::agent_tree::ProductAgentTree) -> Result<T, String> + Send + 'static,
) -> Result<T, LedgerError> {
    let writer = driver.clone();
    let ((result, integrity), effects) = driver
        .mutate(move |tree| {
            let mut integrity = None;
            if tree.revision != tree.stored_revision
                && let Err(error) = writer.save(tree)
            {
                return (Err(error), integrity);
            }
            let result = writer.fold_and_commit(tree, |tree| {
                work(tree).map_err(|message| {
                    integrity = Some(message.clone());
                    crate::agent_tree_protocol::TopologyError::new(
                        crate::agent_tree_protocol::TopologyErrorCode::OperationRecoveryRequired,
                        message,
                    )
                })
            });
            (result, integrity)
        })
        .await
        .map_err(LedgerError::from)?;
    let result = result.map_err(|error| match integrity {
        Some(message) => LedgerError::Integrity(message),
        None => LedgerError::from(error),
    })?;
    driver.settle(effects).await.map_err(LedgerError::from)?;
    Ok(result)
}

/// Prefix of every submit-harness corrective command; its identity is
/// `plak:submit-harness:{agent}:{host_generation}:{attempt}:{round}`.
const SUBMIT_HARNESS_PREFIX: &str = "plak:submit-harness:";

/// The attempt a wake or corrective command was written for: both
/// identities end `{attempt}:{count}`. Other commands do not name theirs.
pub(super) fn command_attempt(command_id: &str) -> Option<&str> {
    let rest = command_id
        .strip_prefix(crate::driver::wake::WakeIdentity::CHILD_PREFIX)
        .or_else(|| command_id.strip_prefix(SUBMIT_HARNESS_PREFIX))
        .or_else(|| {
            command_id
                .strip_prefix(crate::agent_tree::ProductAgentTree::TRANSIENT_RESUME_COMMAND_PREFIX)
        })?;
    let mut parts = rest.rsplitn(3, ':');
    let _count = parts.next()?;
    let attempt = parts.next()?;
    parts.next()?;
    Some(attempt)
}

/// A batch of unread, unclaimed envelopes, in mailbox order: every one for a
/// parked child's wake, every one but results (which ride as
/// `<child_result>` blocks) and the started mail itself for a mail Start.
/// The parent's mail is its instruction and reads as the existing
/// `Message:` delivery; other mail carries the root's untrusted
/// `<child_message>` framing.
fn mail_batch(
    tree: &crate::agent_tree::ProductAgentTree,
    recipient: &str,
    prepared: &mut Prepared,
    include: impl Fn(&crate::agent_tree::MailRecord) -> bool,
) {
    let parent = tree
        .node(recipient)
        .ok()
        .and_then(|node| node.parent_agent_id.clone());
    let mut framed = false;
    for mail in tree.wake_mail(recipient).into_iter().filter(include) {
        let addition = if parent.as_deref() == Some(mail.from.as_str()) {
            format!(
                "{}{}",
                crate::child_prompts::PLAK_CHILD_ASSIGNMENT_MESSAGE_PREFIX,
                mail.body
            )
        } else {
            crate::runtime::result_mail::child_message_block(&mail, !framed)
        };
        if prepared.prompt.len().saturating_add(addition.len()) > crate::sessions::MAX_PROMPT_BYTES
        {
            break;
        }
        framed |= parent.as_deref() != Some(mail.from.as_str());
        prepared.prompt.push_str(&addition);
        prepared.message_ids.push(mail.message_id);
    }
}

/// Writes `command_id`'s intent for `recipient` with its exact text and
/// batch, before submission. A command already written is returned as
/// stored, so an exact resubmission reuses its text and batch; the same
/// identity with another base prompt is refused.
///
/// An unresolved command of the recipient's current attempt that carries a
/// batch (or is a corrective round) refuses a different command until it has
/// a fact or is withdrawn; a wake or corrective round written for an earlier
/// attempt never blocks the current one.
#[allow(
    clippy::too_many_lines,
    reason = "Child delivery preparation validates replay identity and persists the exact command before exposing an admitted attempt to its caller."
)]
/// A [`BatchRule`] a tree writer can own.
enum OwnedBatch {
    WakeAll,
    MailStart(String),
    Delivered(String),
    Other,
}

/// Records one child command's intent and its batch in the tree, or answers
/// the exact command already recorded.
fn prepare_in(
    tree: &mut crate::agent_tree::ProductAgentTree,
    command_id: &str,
    recipient: &str,
    prompt: &str,
    kind: CommandKind,
    batch: &OwnedBatch,
) -> Result<Prepared, String> {
    if let Some(stored) = tree.command(command_id) {
        let exact = stored.recipient == recipient
            && stored
                .text
                .strip_prefix(prompt)
                .is_some_and(|batch| batch.is_empty() || batch.starts_with("\n\n"));
        if !exact {
            return Err("child command id conflicts with a different stored command".to_owned());
        }
        return Ok(Prepared::of(stored));
    }
    let current_attempt = tree
        .attempt(recipient)
        .map(|attempt| attempt.attempt_id.clone());
    // A follow-up reaches a live turn, whose own Start withdrew these.
    if kind != CommandKind::Followup {
        withdraw_ended_continuations(tree, recipient);
    }
    for prior in tree.commands().values().filter(|prior| {
        prior.recipient == recipient
            && prior.kind != AgentCommandKind::RootBatch
            && prior.unresolved()
    }) {
        // A command written for an earlier attempt is never retried
        // onto this one, so it cannot block it.
        if matches!(
            prior.kind,
            AgentCommandKind::Wake | AgentCommandKind::Corrective | AgentCommandKind::Resume
        ) && prior.attempt_id != current_attempt
        {
            continue;
        }
        if !prior.message_ids.is_empty()
            || matches!(
                prior.kind,
                AgentCommandKind::Corrective | AgentCommandKind::Resume
            )
        {
            return Err("prior child result command requires exact acknowledgement reconciliation before another command".to_owned());
        }
    }
    let mut prepared = Prepared {
        prompt: prompt.to_owned(),
        message_ids: Vec::new(),
        instruction: kind.is_instruction(),
    };
    gather_batch(tree, recipient, batch, &mut prepared);
    // Wakes and corrective rounds name their attempt; other commands
    // serve the attempt current at submission.
    let attempt_id = command_attempt(command_id)
        .map(|attempt| AttemptId::from_persisted(attempt.to_owned()))
        .or(current_attempt);
    tree.record_command(AgentCommand::intent(
        command_id,
        recipient,
        attempt_id,
        match kind {
            CommandKind::Start => AgentCommandKind::Start,
            CommandKind::Wake => AgentCommandKind::Wake,
            CommandKind::MailStart => AgentCommandKind::MailStart,
            CommandKind::Followup => AgentCommandKind::Followup,
            CommandKind::Corrective => AgentCommandKind::Corrective,
            CommandKind::Resume => AgentCommandKind::Resume,
            CommandKind::RootStart | CommandKind::RootSteer(_) | CommandKind::HostWake => {
                return Err("root command passed to child ledger".into());
            }
        },
        prepared.prompt.clone(),
        prepared.message_ids.clone(),
    ))
    .map_err(|error| error.message)?;
    Ok(prepared)
}

/// Withdraws each continuation command of `recipient` an ended hold left
/// prepared and unsubmitted, so its batch returns to unread and the next
/// command is not blocked behind it.
pub(super) async fn withdraw_ended_continuation(driver: &TreeDriver, recipient: &str) {
    let recipient = recipient.to_owned();
    let _ = persist_context(&driver.for_agent(&recipient), move |tree| {
        withdraw_ended_continuations(tree, &recipient);
        Ok(())
    })
    .await;
}

/// The ended continuations' withdrawal, inside a mutation. A Start or a
/// wake prepares under the child's startup slot, so no continuation Start
/// is in flight then.
fn withdraw_ended_continuations(tree: &mut crate::agent_tree::ProductAgentTree, recipient: &str) {
    let ended = tree
        .commands()
        .values()
        .filter(|command| {
            command.recipient == recipient
                && command.unresolved()
                && command
                    .command_id
                    .starts_with(crate::agent_tree::ProductAgentTree::LOSS_RESUME_COMMAND_PREFIX)
                && !tree.loss_continuation_admissible(recipient, &command.command_id)
        })
        .map(|command| command.command_id.clone())
        .collect::<Vec<_>>();
    for command_id in ended {
        tree.withdraw_command(&command_id);
    }
}

/// Adds the mail a command carries to its prompt and batch.
fn gather_batch(
    tree: &crate::agent_tree::ProductAgentTree,
    recipient: &str,
    batch: &OwnedBatch,
    prepared: &mut Prepared,
) {
    if matches!(batch, OwnedBatch::WakeAll) {
        mail_batch(tree, recipient, prepared, |_| true);
    } else {
        if let Some(started) = match batch {
            OwnedBatch::MailStart(id) | OwnedBatch::Delivered(id) => tree.mail(id),
            _ => None,
        } && started.to == recipient
            && started.durable == MailDurableState::Accepted
        {
            prepared.message_ids.push(started.message_id.clone());
        }
        // Other mail that is still unread rides the Start too: mail sent
        // to an interrupted child while its turn unwound has no wake
        // to carry it (spec 03 section 9).
        if let OwnedBatch::MailStart(started) = batch {
            mail_batch(tree, recipient, prepared, |mail| {
                &mail.message_id != started
                    && mail.purpose != crate::agent_tree_protocol::MessagePurpose::Result
            });
        }
        for mail in tree.pending_result_mail(recipient) {
            let addition = format!(
                "\n\n<child_result message_id=\"{}\">\n{}\n</child_result>",
                mail.message_id, mail.body
            );
            if prepared.prompt.len().saturating_add(addition.len())
                > crate::sessions::MAX_PROMPT_BYTES
            {
                break;
            }
            prepared.prompt.push_str(&addition);
            prepared.message_ids.push(mail.message_id);
        }
    }
}

async fn prepare(
    driver: &TreeDriver,
    recipient: &str,
    intent: CommandIntent<'_>,
) -> Result<Prepared, LedgerError> {
    let CommandIntent {
        command_id,
        kind,
        base_text: prompt,
        batch,
    } = intent;
    let batch = match batch {
        BatchRule::WakeAll => OwnedBatch::WakeAll,
        BatchRule::MailStart(id) => OwnedBatch::MailStart(id.to_owned()),
        BatchRule::Delivered(id) => OwnedBatch::Delivered(id.to_owned()),
        BatchRule::None | BatchRule::RootResults(_) | BatchRule::Results => OwnedBatch::Other,
    };
    let (command_id, recipient_id, prompt) = (
        command_id.to_owned(),
        recipient.to_owned(),
        prompt.to_owned(),
    );
    persist_context(&driver.for_agent(recipient), move |tree| {
        prepare_in(tree, &command_id, &recipient_id, &prompt, kind, &batch)
    })
    .await
}

/// Whether a command is consumed below retention (spec 03 §14.1). A duplicate
/// joins the feed first: a present acceptance settles from its view, while
/// an absent acceptance below a positive retained boundary is consumed.
/// A reclaimed result is also consumed. No acknowledgement sequence is needed.
///
/// # Errors
///
/// Returns a refused feed read, projection or missing-acceptance integrity error.
pub(super) async fn consumed_below_retention(
    driver: &TreeDriver,
    sessions: &plakous::sessions::AgentSessions,
    session: plakous::model::SessionId,
    command_id: &str,
    disposition: plakous::agent::CommandAckDisposition,
    reclaimed: bool,
) -> Result<bool, String> {
    if reclaimed {
        return Ok(true);
    }
    if disposition != plakous::agent::CommandAckDisposition::Duplicate {
        return Ok(false);
    }
    ChildLedger::settlement_only(driver)
        .duplicate_consumed(sessions, session, command_id)
        .await
        .map_err(Into::into)
}

async fn reconcile_duplicate(
    driver: &TreeDriver,
    sessions: &plakous::sessions::AgentSessions,
    session: plakous::model::SessionId,
    command_id: &str,
) -> Result<bool, LedgerError> {
    let (recipient, cursor) = driver
        .read(|tree| {
            tree.command(command_id).map(|command| {
                (
                    command.recipient.clone(),
                    tree.feed_read_cursor(&session.to_string()),
                )
            })
        })
        .map_err(LedgerError::from)?
        .ok_or_else(|| {
            LedgerError::Integrity(
                "duplicate child command has no intent; integrity refusal".to_owned(),
            )
        })?;
    let feed = crate::agent::read_tree_feed(sessions, session, cursor)
        .await
        .map_err(|error| LedgerError::Unavailable(error.to_string()))?;
    let (projection, _feed) =
        crate::agent::feed_projection::retry_tree_feed(sessions, feed, |feed| {
            crate::agent::feed_projection::apply_on_driver(driver, feed, &recipient)
        })
        .await
        .map_err(LedgerError::from)?;
    let conflicts = !projection.conflicts.is_empty();
    if conflicts {
        return Err(LedgerError::Integrity(
            "duplicate child command has a conflicting turn binding".to_owned(),
        ));
    }
    let (retained_from, tail) = feed_bounds(sessions, session)
        .await
        .map_err(LedgerError::Unavailable)?;
    let (fact, cursor) = driver
        .read(|tree| {
            (
                tree.command(command_id).and_then(|command| command.fact),
                tree.feed_cursor(&session.to_string()),
            )
        })
        .map_err(LedgerError::from)?;
    if cursor < tail {
        return Err(LedgerError::Integrity(
            "duplicate child feed has not reached its tail; projection required".to_owned(),
        ));
    }
    duplicate_consumed(fact, retained_from).map_err(LedgerError::Integrity)
}

fn duplicate_consumed(
    fact: Option<crate::agent_tree::CommandFact>,
    retained_from: u64,
) -> Result<bool, String> {
    crate::driver::ledger::duplicate_consumed(
        match fact {
            Some(
                crate::agent_tree::CommandFact::Accepted | crate::agent_tree::CommandFact::Returned,
            ) => crate::driver::ledger::AcceptanceFact::Present,
            Some(crate::agent_tree::CommandFact::Rejected) => {
                crate::driver::ledger::AcceptanceFact::Rejected
            }
            None => crate::driver::ledger::AcceptanceFact::Absent,
        },
        retained_from,
        "duplicate child command has no matching acceptance; integrity refusal",
    )
    .map_err(Into::into)
}

/// The retained boundary and tail of `session`'s feed.
pub(super) async fn feed_bounds(
    sessions: &plakous::sessions::AgentSessions,
    session: plakous::model::SessionId,
) -> Result<(u64, u64), String> {
    use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest, FeedSeq};
    let page = sessions
        .feed_page(FeedPageRequest {
            cursor: FeedCursor::new(session, FeedSeq::new(0)),
            max_records: std::num::NonZeroUsize::MIN,
        })
        .await
        .map_err(|error| format!("the feed's retention could not be read: {error}"))?;
    match page {
        FeedPage::Records {
            retained_from,
            tail,
            ..
        }
        | FeedPage::BehindRetention {
            retained_from,
            tail,
            ..
        } => Ok((retained_from.get(), tail.get())),
        _ => Err("the feed's retention could not be read".to_owned()),
    }
}

/// Validates feed settlement or writes the host's no-fact decision.
async fn acknowledge(
    driver: &TreeDriver,
    command_id: &str,
    submission: Settlement,
) -> Result<(), LedgerError> {
    let recipient = driver
        .read(|tree| {
            tree.command(command_id)
                .map(|command| command.recipient.clone())
        })
        .map_err(LedgerError::from)?;
    let driver = recipient
        .as_deref()
        .map_or_else(|| driver.clone(), |recipient| driver.for_agent(recipient));
    let command_id = command_id.to_owned();
    persist_context(&driver, move |tree| {
        use crate::agent_tree::CommandFact;
        let command_id = command_id.as_str();
        match submission {
            Settlement::Recorded => match tree.command(command_id).and_then(|command| command.fact)
            {
                Some(CommandFact::Accepted | CommandFact::Returned) => Ok(()),
                _ => {
                    Err("child admission has no projected acceptance; integrity refusal".to_owned())
                }
            },
            Settlement::Consumed => {
                let recipient = tree
                    .command(command_id)
                    .ok_or_else(|| "consumed command has no intent".to_owned())?
                    .recipient
                    .clone();
                tree.record_consumed_command(command_id)
                    .map_err(|error| error.message)?;
                tree.project_mail_decisions(&recipient)
                    .map_err(|error| error.message)
            }
            Settlement::Refused { .. } => {
                if tree.command(command_id).is_some_and(|command| {
                    matches!(
                        command.fact,
                        Some(CommandFact::Accepted | CommandFact::Returned)
                    )
                }) {
                    return Err("a recorded child admission cannot be withdrawn".to_owned());
                }
                tree.withdraw_command(command_id);
                Ok(())
            }
        }
    })
    .await
}

/// One child command written and not yet settled.
pub(super) struct JournaledCommand {
    pub(super) command_id: String,
}

impl JournaledCommand {
    /// Settles the command without submitting it.
    pub(super) async fn settle(&self, driver: &TreeDriver, accepted: bool) -> Result<(), String> {
        let submission = if accepted {
            Settlement::Consumed
        } else {
            Settlement::Refused { pause: None }
        };
        ChildLedger::settlement_only(driver)
            .settle(&self.command_id, submission)
            .await
            .map_err(Into::into)
    }
}

/// Every unresolved command written for `recipient`.
pub(super) fn prepared_commands(
    driver: &TreeDriver,
    recipient: &str,
) -> Result<Vec<JournaledCommand>, String> {
    driver
        .read(|tree| {
            tree.commands()
                .values()
                .filter(|command| {
                    command.recipient == recipient
                        && command.kind != AgentCommandKind::RootBatch
                        && command.unresolved()
                })
                .map(|command| JournaledCommand {
                    command_id: command.command_id.clone(),
                })
                .collect()
        })
        .map_err(|error| error.message)
}

/// The command `recipient`'s feed accepted as `turn`, if one did.
pub(super) fn accepted_command(
    driver: &TreeDriver,
    recipient: &str,
    turn: plakous::model::TurnId,
) -> Result<Option<String>, String> {
    let turn = turn.to_string();
    driver
        .read(|tree| {
            tree.commands()
                .values()
                .find(|command| {
                    command.recipient == recipient
                        && command.kind != AgentCommandKind::RootBatch
                        && command.fact == Some(crate::agent_tree::CommandFact::Accepted)
                        && command.turn_id.as_deref() == Some(turn.as_str())
                })
                .map(|command| command.command_id.clone())
        })
        .map_err(|error| error.message)
}

/// Applies `recipient`'s feed to its tail through the tree projector (spec
/// 03 §14.1, §14.3): every command the feed names gets its fact, accepted
/// batches are acknowledged and accepted commands bind their turns. A
/// command with neither fact afterwards was never recorded.
pub(super) async fn project_child_feed(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    recipient: &str,
    attached: &crate::agent::AttachedCodingSession,
) -> Result<bool, String> {
    project_feed(
        runtime,
        recipient,
        attached.sessions(),
        attached.locator().session_id(),
    )
    .await
}

/// [`project_child_feed`] for a session by id, with no lease. Returns
/// whether the pass went behind the feed's retention.
pub(super) async fn project_feed(
    runtime: &crate::topology_runtime::ProductTopologyRuntime,
    recipient: &str,
    sessions: &plakous::sessions::AgentSessions,
    session: plakous::model::SessionId,
) -> Result<bool, String> {
    let key = session.to_string();
    let cursor = runtime
        .read(|tree| tree.feed_read_cursor(&key))
        .map_err(|error| error.message)?;
    let feed = crate::agent::read_tree_feed(sessions, session, cursor).await?;
    let (projection, feed) =
        crate::agent::feed_projection::retry_tree_feed(sessions, feed, |feed| {
            crate::agent::feed_projection::apply_on_runtime(runtime, feed, recipient)
        })
        .await
        .map_err(|error| error.message)?;
    let conflicts = projection.conflicts;
    if let Some(conflict) = conflicts.first() {
        return Err(format!(
            "accepted child command {} names turn {} while its attempt runs turn {}; recovery \
             required",
            conflict.command_id, conflict.recovered_turn_id, conflict.bound_turn_id
        ));
    }
    Ok(feed.went_behind_retention())
}
