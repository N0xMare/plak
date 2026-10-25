//! Child commands as `agent_commands` intents (spec 03 §14.1): every child
//! Start, wake, mail Start, followup and corrective round is written to the
//! tree with its exact text and message batch before it is submitted. What
//! became of it is the child feed's `CommandAccepted` or `CommandRejected`
//! fact, which only the tree projector applies. The submission site validates
//! that view, records below-retention consumption, or withdraws a command the
//! runtime definitely refused before admitting it.

use crate::agent_tree::{AgentCommand, AgentCommandKind, MailDurableState};
use crate::agent_tree_protocol::AttemptId;
use crate::agent_tree_tools::TreeDriver;

/// A command's exact text and the mail its batch carries.
#[derive(Clone, Debug)]
pub(super) struct PreparedResultContext {
    pub(super) prompt: String,
    message_ids: Vec<String>,
}

impl PreparedResultContext {
    fn of(command: &AgentCommand) -> Self {
        Self {
            prompt: command.text.clone(),
            message_ids: command.message_ids.clone(),
        }
    }
}

fn persist_context<T>(
    driver: &TreeDriver,
    work: impl FnOnce(&mut crate::agent_tree::ProductAgentTree) -> Result<T, String>,
) -> Result<T, String> {
    let (result, effects) = driver
        .mutate(|tree| {
            if tree.revision != tree.stored_revision {
                driver.save(tree)?;
            }
            driver.fold_and_commit(tree, |tree| {
                work(tree).map_err(|message| {
                    crate::agent_tree_protocol::TopologyError::new(
                        crate::agent_tree_protocol::TopologyErrorCode::OperationRecoveryRequired,
                        message,
                    )
                })
            })
        })
        .map_err(|error| error.message)?;
    let result = result.map_err(|error| error.message)?;
    driver.settle(effects).map_err(|error| error.message)?;
    Ok(result)
}

fn is_wake(command_id: &str) -> bool {
    command_id.starts_with(super::park::WAKE_COMMAND_PREFIX)
}

/// The mail a mail-delivered Start (`plak:mail:{id}:start`) carries in its
/// prompt. Its batch names it, so an accepted settlement (including an exact
/// retry after a reopen) acknowledges it like the result mail beside it.
fn started_mail(command_id: &str) -> Option<&str> {
    command_id
        .strip_prefix("plak:mail:")?
        .strip_suffix(":start")
        .filter(|id| !id.is_empty())
}

fn delivered_mail(command_id: &str) -> Option<&str> {
    let mail = command_id.strip_prefix("plak:mail:")?;
    [":start", ":follow-up", ":steer"]
        .into_iter()
        .find_map(|suffix| mail.strip_suffix(suffix))
        .filter(|id| !id.is_empty())
}

/// Prefix of every submit-harness corrective command; its identity is
/// `plak:submit-harness:{agent}:{host_generation}:{attempt}:{round}`.
pub(super) const TRANSIENT_RESUME_PREFIX: &str = "plak:resume-transient:";

const SUBMIT_HARNESS_PREFIX: &str = "plak:submit-harness:";

/// Prefix of a spawn's first Start.
const SPAWN_START_PREFIX: &str = "plak:spawn-start:";

/// The attempt a wake or corrective command was written for: both
/// identities end `{attempt}:{count}`. Other commands do not name theirs.
pub(super) fn command_attempt(command_id: &str) -> Option<&str> {
    let rest = command_id
        .strip_prefix(super::park::WAKE_COMMAND_PREFIX)
        .or_else(|| command_id.strip_prefix(SUBMIT_HARNESS_PREFIX))
        .or_else(|| command_id.strip_prefix(TRANSIENT_RESUME_PREFIX))?;
    let mut parts = rest.rsplitn(3, ':');
    let _count = parts.next()?;
    let attempt = parts.next()?;
    parts.next()?;
    Some(attempt)
}

/// Which child command an identity names.
fn command_kind(command_id: &str) -> AgentCommandKind {
    if is_wake(command_id) {
        AgentCommandKind::Wake
    } else if command_id.starts_with(SUBMIT_HARNESS_PREFIX) {
        AgentCommandKind::Corrective
    } else if command_id.starts_with(TRANSIENT_RESUME_PREFIX) {
        AgentCommandKind::Resume
    } else if started_mail(command_id).is_some() {
        AgentCommandKind::MailStart
    } else if command_id.starts_with(SPAWN_START_PREFIX) {
        AgentCommandKind::Start
    } else {
        AgentCommandKind::Followup
    }
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
    prepared: &mut PreparedResultContext,
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
pub(super) fn prepare(
    driver: &TreeDriver,
    recipient: &str,
    command_id: &str,
    prompt: &str,
) -> Result<PreparedResultContext, String> {
    persist_context(&driver.for_agent(recipient), |tree| {
        if let Some(stored) = tree.command(command_id) {
            let exact = stored.recipient == recipient
                && stored
                    .text
                    .strip_prefix(prompt)
                    .is_some_and(|batch| batch.is_empty() || batch.starts_with("\n\n"));
            if !exact {
                return Err("child command id conflicts with a different stored command".to_owned());
            }
            return Ok(PreparedResultContext::of(stored));
        }
        let current_attempt = tree
            .attempt(recipient)
            .map(|attempt| attempt.attempt_id.clone());
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
        let mut prepared = PreparedResultContext {
            prompt: prompt.to_owned(),
            message_ids: Vec::new(),
        };
        if is_wake(command_id) || command_id.starts_with(TRANSIENT_RESUME_PREFIX) {
            mail_batch(tree, recipient, &mut prepared, |_| true);
        } else {
            if let Some(started) = delivered_mail(command_id).and_then(|id| tree.mail(id))
                && started.to == recipient
                && started.durable == MailDurableState::Accepted
            {
                prepared.message_ids.push(started.message_id.clone());
            }
            // Other mail that is still unread rides the Start too: mail sent
            // to an interrupted child while its turn unwound has no wake
            // to carry it (spec 03 section 9).
            if let Some(started) = started_mail(command_id) {
                mail_batch(tree, recipient, &mut prepared, |mail| {
                    mail.message_id != started
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
        // Wakes and corrective rounds name their attempt; other commands
        // serve the attempt current at submission.
        let attempt_id = command_attempt(command_id)
            .map(|attempt| AttemptId::from_persisted(attempt.to_owned()))
            .or(current_attempt);
        tree.record_command(AgentCommand::intent(
            command_id,
            recipient,
            attempt_id,
            command_kind(command_id),
            prepared.prompt.clone(),
            prepared.message_ids.clone(),
        ))
        .map_err(|error| error.message)?;
        Ok(prepared)
    })
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
    let (recipient, cursor) = driver
        .read(|tree| {
            tree.command(command_id).map(|command| {
                (
                    command.recipient.clone(),
                    tree.feed_read_cursor(&session.to_string()),
                )
            })
        })
        .map_err(|error| error.message)?
        .ok_or_else(|| "duplicate child command has no intent; integrity refusal".to_owned())?;
    let feed = crate::agent::read_tree_feed(sessions, session, cursor).await?;
    let (projection, _feed) = super::feed_projection::retry_tree_feed(sessions, feed, |feed| {
        super::feed_projection::apply_on_driver(driver, feed, &recipient)
    })
    .await
    .map_err(|error| error.message)?;
    let conflicts = !projection.conflicts.is_empty();
    if conflicts {
        return Err("duplicate child command has a conflicting turn binding".to_owned());
    }
    let (retained_from, tail) = feed_bounds(sessions, session).await?;
    let (fact, cursor) = driver
        .read(|tree| {
            (
                tree.command(command_id).and_then(|command| command.fact),
                tree.feed_cursor(&session.to_string()),
            )
        })
        .map_err(|error| error.message)?;
    if cursor < tail {
        return Err(
            "duplicate child feed has not reached its tail; projection required".to_owned(),
        );
    }
    duplicate_consumed(fact, retained_from)
}

fn duplicate_consumed(
    fact: Option<crate::agent_tree::CommandFact>,
    retained_from: u64,
) -> Result<bool, String> {
    match fact {
        Some(
            crate::agent_tree::CommandFact::Accepted | crate::agent_tree::CommandFact::Returned,
        ) => Ok(false),
        None if retained_from > 0 => Ok(true),
        _ => {
            Err("duplicate child command has no matching acceptance; integrity refusal".to_owned())
        }
    }
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

/// What a child command's submission learned.
#[derive(Clone, Copy, Debug)]
pub(super) enum Submission {
    /// The feed already recorded admission; validate its projected outcome.
    Recorded,
    /// Exact resubmission proved consumption below retention, without a turn.
    Consumed,
    /// Definitely refused before admission: withdraw, leaving its mail unread.
    Refused,
}

/// Validates feed settlement or writes the host's no-fact decision.
pub(super) fn acknowledge(
    driver: &TreeDriver,
    command_id: &str,
    submission: Submission,
) -> Result<(), String> {
    let recipient = driver
        .read(|tree| {
            tree.command(command_id)
                .map(|command| command.recipient.clone())
        })
        .map_err(|error| error.message)?;
    let driver = recipient
        .as_deref()
        .map_or_else(|| driver.clone(), |recipient| driver.for_agent(recipient));
    persist_context(&driver, |tree| {
        use crate::agent_tree::CommandFact;
        match submission {
            Submission::Recorded => match tree.command(command_id).and_then(|command| command.fact)
            {
                Some(CommandFact::Accepted | CommandFact::Returned) => Ok(()),
                _ => {
                    Err("child admission has no projected acceptance; integrity refusal".to_owned())
                }
            },
            Submission::Consumed => {
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
            Submission::Refused => {
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
}

/// One child command written and not yet settled.
pub(super) struct JournaledCommand {
    pub(super) command_id: String,
    kind: AgentCommandKind,
    attempt_id: Option<AttemptId>,
    pub(super) context: PreparedResultContext,
}

impl JournaledCommand {
    /// Whether the command is a corrective round.
    pub(super) fn is_corrective(&self) -> bool {
        self.kind == AgentCommandKind::Corrective
    }

    /// Whether the command was written for an attempt other than `current`.
    pub(super) fn for_other_attempt(&self, current: Option<&AttemptId>) -> bool {
        self.attempt_id.as_ref() != current
    }

    /// Settles the command without submitting it.
    pub(super) fn settle(&self, driver: &TreeDriver, accepted: bool) -> Result<(), String> {
        let submission = if accepted {
            Submission::Consumed
        } else {
            Submission::Refused
        };
        acknowledge(driver, &self.command_id, submission)
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
                    kind: command.kind,
                    attempt_id: command.attempt_id.clone(),
                    context: PreparedResultContext::of(command),
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
    attached: &super::AttachedCodingSession,
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
    let (projection, feed) = super::feed_projection::retry_tree_feed(sessions, feed, |feed| {
        super::feed_projection::apply_on_runtime(runtime, feed, recipient)
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

#[cfg(test)]
mod tests;
