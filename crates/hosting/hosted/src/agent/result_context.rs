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
    persist_context(driver, |tree| {
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
    persist_context(driver, |tree| {
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
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::agent_tree::{ProductAgentTree, TreeStore as _};
    use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest, WriterMode};

    struct SecondSaveStore {
        inner: crate::agent_tree::MemoryTreeStore,
        saves: std::sync::atomic::AtomicUsize,
        stale: bool,
    }

    impl crate::agent_tree::TreeStore for SecondSaveStore {
        fn exists(&self, id: &str) -> Result<bool, crate::agent_tree::TreeStoreError> {
            self.inner.exists(id)
        }
        fn ids(&self) -> Result<Vec<String>, crate::agent_tree::TreeStoreError> {
            self.inner.ids()
        }
        fn load(
            &self,
            id: &str,
        ) -> Result<Option<crate::agent_tree::PersistedTree>, crate::agent_tree::TreeStoreError>
        {
            self.inner.load(id)
        }
        fn save(
            &self,
            tree: &ProductAgentTree,
        ) -> Result<crate::agent_tree::Revision, crate::agent_tree::TreeStoreError> {
            if self.saves.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                return Err(if self.stale {
                    crate::agent_tree::TreeStoreError::stale(
                        tree.stored_revision + 1,
                        tree.revision,
                    )
                } else {
                    crate::agent_tree::TreeStoreError::unavailable("second save refused")
                });
            }
            self.inner.save(tree)
        }
    }

    #[test]
    fn duplicate_projection_second_save_refusal_restores_command_and_cursor() {
        use plakous::sessions::feed::{FeedFact, FeedRecord};
        for stale in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (mut tree, child) = root_with_child(dir.path(), "duplicate-second-save");
            tree.record_command(AgentCommand::intent(
                "duplicate",
                &child,
                tree.attempt(&child)
                    .map(|attempt| attempt.attempt_id.clone()),
                AgentCommandKind::Start,
                "assignment".to_owned(),
                Vec::new(),
            ))
            .unwrap();
            let store = std::sync::Arc::new(SecondSaveStore {
                inner: crate::agent_tree::MemoryTreeStore::new(),
                saves: std::sync::atomic::AtomicUsize::new(0),
                stale,
            });
            let driver = TreeDriver::new(
                std::sync::Arc::new(std::sync::Mutex::new(tree.clone())),
                store,
                tree.vcs_port(),
                None,
            );
            let header = serde_json::from_value(serde_json::json!({
                "session_id": "00000000-0000-4000-8000-000000000001", "seq": 1,
                "branch_id": "00000000-0000-4000-8000-000000000002", "committed_at_ms": 1000,
                "change": {"created": {"genesis": "00000000-0000-4000-8000-000000000003", "revision": 1}}
            })).unwrap();
            let record = FeedRecord::from_store(
                header,
                vec![FeedFact::CommandAccepted {
                    command_id: plakous::agent::CommandId::new("duplicate").unwrap(),
                    kind: plakous::sessions::feed::FeedCommandKind::Root {
                        origin: plakous::model::TurnOrigin::UserPrompt,
                    },
                    turn_id: None,
                    sequence: 1,
                    operation_id: None,
                }],
            );
            // Intent publication is the first save; the duplicate's projection is the second.
            driver
                .save(&mut driver.tree_handle_for_test().lock().unwrap())
                .unwrap();
            let project = |tree: &mut ProductAgentTree| {
                tree.project_feed(
                    &child,
                    "00000000-0000-4000-8000-000000000001",
                    std::slice::from_ref(&record),
                    &crate::agent_tree::TreeFeedReads::default(),
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
            };
            assert!(persist_context(&driver, project).is_err());
            driver
                .read(|restored| {
                    assert_eq!(restored.command("duplicate"), tree.command("duplicate"));
                    assert_eq!(restored.node(&child), tree.node(&child));
                    assert_eq!(
                        restored.feed_cursor("00000000-0000-4000-8000-000000000001"),
                        0
                    );
                })
                .unwrap();
            persist_context(&driver, project).unwrap();
            assert_eq!(
                driver
                    .read(|tree| tree.command("duplicate").unwrap().fact)
                    .unwrap(),
                Some(crate::agent_tree::CommandFact::Accepted)
            );
        }
    }

    fn project_acceptance(
        runtime: &crate::topology_runtime::ProductTopologyRuntime,
        recipient: &str,
        command: &str,
    ) {
        use plakous::sessions::feed::{FeedFact, FeedRecord};
        let header = serde_json::from_value(serde_json::json!({
            "session_id": "00000000-0000-4000-8000-000000000001", "seq": 1,
            "branch_id": "00000000-0000-4000-8000-000000000002", "committed_at_ms": 1000,
            "change": {"created": {"genesis": "00000000-0000-4000-8000-000000000003", "revision": 1}}
        })).unwrap();
        let record = FeedRecord::from_store(
            header,
            vec![FeedFact::CommandAccepted {
                command_id: plakous::agent::CommandId::new(command).unwrap(),
                kind: plakous::sessions::feed::FeedCommandKind::Root {
                    origin: plakous::model::TurnOrigin::UserPrompt,
                },
                turn_id: None,
                sequence: 1,
                operation_id: None,
            }],
        );
        runtime
            .mutate_then_emit(false, |tree| {
                tree.project_feed(
                    recipient,
                    "00000000-0000-4000-8000-000000000001",
                    &[record],
                    &crate::agent_tree::TreeFeedReads::default(),
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
            })
            .unwrap();
    }

    #[test]
    fn duplicate_child_commands_settle_from_views_without_ack_feed_sequences() {
        use crate::agent_tree::CommandFact;
        for retained_from in [0, 1, 100] {
            assert_eq!(
                duplicate_consumed(Some(CommandFact::Accepted), retained_from),
                Ok(false)
            );
            assert_eq!(
                duplicate_consumed(Some(CommandFact::Returned), retained_from),
                Ok(false)
            );
            assert!(duplicate_consumed(Some(CommandFact::Rejected), retained_from).is_err());
        }
        assert!(duplicate_consumed(None, 0).is_err());
        assert_eq!(duplicate_consumed(None, 1), Ok(true));
        assert_eq!(duplicate_consumed(None, 100), Ok(true));
    }

    fn complete(tree: &mut ProductAgentTree) {
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        tree.submit_result(
            &child.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.to_owned(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
    }

    #[tokio::test]
    async fn prepared_child_context_is_exact_with_new_mail_and_requires_ack_before_new_command() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            &id,
            "root",
            "owner",
            dir.path().to_owned(),
            "selected",
            WriterMode::SingleLease,
        )
        .unwrap();
        complete(&mut tree);
        let store = std::sync::Arc::new(crate::agent_tree::MemoryTreeStore::new());
        let runtime =
            crate::topology_runtime::insert_root_with(id.clone(), tree, store.clone(), None)
                .unwrap();
        let prepared = prepare(runtime.driver(), "root", "command-one", "continue").unwrap();
        runtime
            .mutate_then_emit(false, |tree| {
                complete(tree);
                Ok(())
            })
            .unwrap();
        crate::topology_runtime::force_release(&id).await;
        let persisted = store.load(&id).unwrap().unwrap();
        let runtime = crate::topology_runtime::insert_root_with(
            id.clone(),
            ProductAgentTree::reopen(persisted),
            store,
            None,
        )
        .unwrap();
        let replay = prepare(runtime.driver(), "root", "command-one", "continue").unwrap();
        assert_eq!(prepared.prompt, replay.prompt);
        assert_eq!(replay.message_ids.len(), 1);
        assert!(prepare(runtime.driver(), "root", "command-one", "changed").is_err());
        assert!(prepare(runtime.driver(), "root", "command-two", "continue").is_err());
        project_acceptance(&runtime, "root", "command-one");
        acknowledge(runtime.driver(), "command-one", Submission::Recorded).unwrap();
        acknowledge(runtime.driver(), "command-one", Submission::Recorded).unwrap();
        let second = prepare(runtime.driver(), "root", "command-two", "continue").unwrap();
        assert_eq!(second.message_ids.len(), 1);
        assert_ne!(second.message_ids, prepared.message_ids);
        acknowledge(runtime.driver(), "command-two", Submission::Refused).unwrap();
        let third = prepare(runtime.driver(), "root", "command-three", "continue").unwrap();
        assert_eq!(
            third.message_ids, second.message_ids,
            "definitively rejected commands do not consume results"
        );
        assert!(
            acknowledge(runtime.driver(), "command-two", Submission::Recorded).is_err(),
            "a terminal rejected context cannot change outcome"
        );
        crate::topology_runtime::force_release(&id).await;
    }

    fn parent_mail(
        agent_id: &str,
        message: &str,
    ) -> crate::agent_tree_protocol::SendAgentMessageRequest {
        crate::agent_tree_protocol::SendAgentMessageRequest {
            agent_id: agent_id.to_owned(),
            message: message.to_owned(),
            purpose: None,
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        }
    }

    fn root_with_child(dir: &std::path::Path, id: &str) -> (ProductAgentTree, String) {
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            id,
            "root",
            "owner",
            dir.to_owned(),
            "selected",
            WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap()
            .agent_id;
        (tree, child)
    }

    /// A mail Start also carries the other mail still unread for the child
    /// (an interrupted child's mail has no wake to carry it), and its
    /// accepted settlement acknowledges all of it.
    #[tokio::test]
    async fn a_mail_start_carries_other_unread_mail_and_acknowledges_it() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let (mut tree, child) = root_with_child(dir.path(), &id);
        let earlier = tree
            .send_agent_message("root", &parent_mail(&child, "use the v2 schema"), None)
            .unwrap();
        let started = tree
            .send_agent_message("root", &parent_mail(&child, "resume"), None)
            .unwrap();
        let runtime = crate::topology_runtime::insert_root(id.clone(), tree, None).unwrap();
        let command = format!("plak:mail:{}:start", started.message_id);
        let prepared = prepare(
            runtime.driver(),
            &child,
            &command,
            "assignment\n\nMessage:\nresume",
        )
        .unwrap();
        assert_eq!(
            prepared.prompt,
            "assignment\n\nMessage:\nresume\n\nMessage:\nuse the v2 schema"
        );
        assert_eq!(
            prepared.message_ids,
            [started.message_id.clone(), earlier.message_id.clone()]
        );
        project_acceptance(&runtime, &child, &command);
        acknowledge(runtime.driver(), &command, Submission::Recorded).unwrap();
        assert!(
            runtime
                .read(|tree| tree.wake_mail(&child).is_empty())
                .unwrap()
        );
        crate::topology_runtime::force_release(&id).await;
    }

    #[tokio::test]
    async fn followup_and_steer_primary_mail_acknowledgement_requires_a_feed_fact() {
        for kind in ["follow-up", "steer"] {
            let dir = tempfile::tempdir().unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            let (mut tree, child) = root_with_child(dir.path(), &id);
            let sent = tree
                .send_agent_message("root", &parent_mail(&child, "status?"), None)
                .unwrap();
            let runtime = crate::topology_runtime::insert_root(id.clone(), tree, None).unwrap();
            let command = format!("plak:mail:{}:{kind}", sent.message_id);
            let prepared = prepare(runtime.driver(), &child, &command, "status?").unwrap();
            assert_eq!(prepared.message_ids, std::slice::from_ref(&sent.message_id));
            assert!(acknowledge(runtime.driver(), &command, Submission::Recorded).is_err());
            assert_eq!(
                runtime
                    .read(|tree| tree.mail(&sent.message_id).unwrap().durable)
                    .unwrap(),
                MailDurableState::Accepted
            );
            project_acceptance(&runtime, &child, &command);
            acknowledge(runtime.driver(), &command, Submission::Recorded).unwrap();
            assert_eq!(
                runtime
                    .read(|tree| tree.mail(&sent.message_id).unwrap().durable)
                    .unwrap(),
                MailDurableState::Acknowledged
            );
            crate::topology_runtime::force_release(&id).await;
        }
    }

    /// A batch left prepared by an earlier attempt's wake never blocks the
    /// current attempt's commands; one of the current attempt still does.
    #[tokio::test]
    async fn another_attempts_prepared_wake_does_not_block_a_new_command() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let (mut tree, child) = root_with_child(dir.path(), &id);
        tree.send_agent_message("root", &parent_mail(&child, "status?"), None)
            .unwrap();
        let attempt = tree.attempt(&child).unwrap().attempt_id.as_str().to_owned();
        let runtime = crate::topology_runtime::insert_root(id.clone(), tree, None).unwrap();
        let stale = format!(
            "{}{child}:1:{child}-a9:3",
            super::super::park::WAKE_COMMAND_PREFIX
        );
        assert_eq!(
            command_attempt(&stale),
            Some(format!("{child}-a9").as_str())
        );
        let held = prepare(runtime.driver(), &child, &stale, "wake").unwrap();
        // The attempt that journaled it ended; the current one carries on.
        assert_eq!(held.message_ids.len(), 1, "the stale wake holds the mail");
        prepare(
            runtime.driver(),
            &child,
            "plak:spawn-start:t:c",
            "assignment",
        )
        .unwrap();
        let current = format!(
            "{}{child}:1:{attempt}:3",
            super::super::park::WAKE_COMMAND_PREFIX
        );
        assert_eq!(command_attempt(&current), Some(attempt.as_str()));
        let blocking = prepare(runtime.driver(), &child, &current, "wake").unwrap();
        assert_eq!(blocking.message_ids.len(), 1);
        assert!(
            prepare(runtime.driver(), &child, "plak:other:command", "assignment").is_err(),
            "a prepared batch of the current attempt still blocks"
        );
        crate::topology_runtime::force_release(&id).await;
    }

    #[tokio::test]
    async fn command_prepared_without_results_does_not_gain_new_results_on_retry() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let tree = ProductAgentTree::bootstrap_root_for_test(
            &id,
            "root",
            "owner",
            dir.path().to_owned(),
            "selected",
            WriterMode::SingleLease,
        )
        .unwrap();
        let runtime = crate::topology_runtime::insert_root(id.clone(), tree, None).unwrap();
        let prepared = prepare(runtime.driver(), "root", "command-one", "continue").unwrap();
        runtime
            .mutate_then_emit(false, |tree| {
                complete(tree);
                Ok(())
            })
            .unwrap();
        let replay = prepare(runtime.driver(), "root", "command-one", "continue").unwrap();
        assert_eq!(prepared.prompt, replay.prompt);
        assert_eq!(replay.message_ids, [] as [String; 0]);
        assert_eq!(
            runtime
                .read(|tree| tree.pending_result_mail("root").len())
                .unwrap(),
            1
        );
        crate::topology_runtime::force_release(&id).await;
    }
}
