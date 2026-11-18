//! `ChooseAccount` on an idle root (spec 02 §4.4.1). A non-root kind is a
//! record write. The root kind continues its line through the rebind
//! workflow; a continue then submits one ordinary root-start on the line head.

use super::{CliFailure, InterruptWatch, RootActor, commit_session_update, surface};
use crate::runtime::rebind::{Rebound, handle_account_rebind};
use crate::sessions::projector::{LastRootOutcome, LastRootTurn};
use crate::sessions::{NoticeScope, SessionRecord, TranscriptRow};
use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::status::{HostPhase, HostStatusDetail};
use surface::{AccountChoiceOutcome, AccountChoiceRefusal, SurfaceAccountChoice};

/// The root-start a continue submits when a round committed after the failed
/// prompt. Pinned: spec 02 §4.4.1.
pub const CONTINUATION_PROMPT: &str = "Continue the previous request from where it stopped. The tool results above are what actually ran.";

/// The workflow operation prefix of a continue's rebind.
const CONTINUE_OPERATION: &str = "plak:rebind:account:";

/// The exactly-once identities of continuing one failed root turn on one
/// account: the rebind's workflow operation and the root-start's queue item,
/// whose command id is `plak.queue.<item>.1`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContinueIds {
    pub(crate) operation_id: String,
    pub(crate) item_id: String,
}

impl ContinueIds {
    pub(crate) fn of(turn_id: &str, account: &str) -> Self {
        let digest = crate::sessions::digest_parts(&["plak:account-continue", turn_id, account]);
        let mut bytes = [0_u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&digest[index * 2..index * 2 + 2], 16).unwrap_or(0);
        }
        Self {
            operation_id: format!("{CONTINUE_OPERATION}{digest}"),
            item_id: uuid::Builder::from_random_bytes(bytes)
                .into_uuid()
                .hyphenated()
                .to_string(),
        }
    }

    pub(crate) fn command_id(&self) -> String {
        format!("plak.queue.{}.1", self.item_id)
    }
}

/// The text a continue submits for `turn`: its exact prompt when nothing
/// committed after it, otherwise the pinned continuation.
fn continue_text(turn: &LastRootTurn) -> String {
    match (&turn.prompt, turn.committed_after_prompt) {
        (Some(prompt), false) if !prompt.trim().is_empty() => prompt.clone(),
        _ => CONTINUATION_PROMPT.to_owned(),
    }
}

/// The recovery notice for a continue whose rebind published and whose
/// root-start was never submitted: the line head is still the continue's
/// successor, its last root turn is still the failed one, and no queue item
/// or delivery carries the derived command. Reopen never resubmits it.
pub(crate) fn unsent_continue_notice(
    session: &SessionRecord,
    views: &crate::sessions::projector::SessionViews,
) -> Option<TranscriptRow> {
    let operation = session.source_workflow_operation_id.as_deref()?;
    let account = session.account.as_deref()?;
    let turn = views.state.last_root_turn.as_ref()?;
    let ids = ContinueIds::of(&turn.turn_id, account);
    if operation != ids.operation_id || turn.outcome != Some(LastRootOutcome::Failed) {
        return None;
    }
    let command_id = ids.command_id();
    if views.state.deliveries.contains_key(&command_id)
        || session
            .pending_queue
            .iter()
            .any(|item| item.id == ids.item_id)
    {
        return None;
    }
    Some(
        TranscriptRow::warning(
            NoticeScope::Recovery,
            format!(
                "the session continued on {account}, but its continue prompt was not submitted; \
                 nothing was resent"
            ),
        )
        .with_remedy(
            plakous_host_protocol::notice_remedy::NoticeRemedy::ContinueOnAccount {
                account: account.to_owned(),
                command_id,
            },
        ),
    )
}

type Selected = (String, Option<crate::sessions::queue::QueueItem>, bool);

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    /// Applies a validated account choice on the idle root. A continue's
    /// root-start is returned for the loop to drive, as a typed prompt is.
    pub(super) async fn choose_account(
        &mut self,
        effect: surface::SurfaceAccountEffect,
    ) -> Option<Selected> {
        let surface::SurfaceAccountEffect { choice, finish } = effect;
        let (outcome, selected) = match choice {
            SurfaceAccountChoice::Store { kind, account } => {
                (self.store_choice(kind, account), None)
            }
            SurfaceAccountChoice::Root {
                account,
                continue_failed,
            } => match self.continue_root(&account, continue_failed).await {
                Ok(selected) => (self.chosen(), selected),
                Err(refusal) => (AccountChoiceOutcome::Refused(refusal), None),
            },
        };
        if let AccountChoiceOutcome::Refused(refusal) = &outcome {
            let _ = self.role.events.notice(TranscriptRow::error(
                NoticeScope::Settings,
                refusal_note(refusal),
            ));
        }
        finish(outcome);
        selected
    }

    fn chosen(&self) -> AccountChoiceOutcome {
        AccountChoiceOutcome::Chosen {
            root: self.role.config.session.account.clone(),
            choices: self.role.config.session.account_choices.clone(),
        }
    }

    fn store_choice(&mut self, kind: String, account: String) -> AccountChoiceOutcome {
        let saved = commit_session_update(
            &*self.role.config.store,
            &self.role.config.home,
            &mut self.role.config.session,
            |candidate| {
                if candidate.account_choices.get(&kind) == Some(&account) {
                    return false;
                }
                candidate.account_choices.insert(kind, account);
                candidate.validate_for_store().is_ok()
            },
        );
        match saved {
            Ok(_) => self.chosen(),
            Err(error) => AccountChoiceOutcome::Refused(AccountChoiceRefusal::Failed(
                CliFailure::new(format!("the account choice could not be saved: {error}"))
                    .host_failure(),
            )),
        }
    }

    /// The failed root turn a continue resubmits, read from the line head's
    /// projected lifecycle facts.
    async fn failed_root_turn(&self) -> Result<LastRootTurn, AccountChoiceRefusal> {
        let views = crate::runtime::projection::project_to_tail(
            self.opened.sessions(),
            &self.role.config.store,
            &self.role.config.session.id,
            self.opened.locator(),
        )
        .await
        .map_err(|error| {
            AccountChoiceRefusal::Failed(
                CliFailure::new(format!(
                    "the session's last root turn could not be read: {error}"
                ))
                .host_failure(),
            )
        })?;
        views
            .state
            .last_root_turn
            .filter(|turn| turn.outcome == Some(LastRootOutcome::Failed))
            .ok_or(AccountChoiceRefusal::NoFailedTurn)
    }

    async fn continue_root(
        &mut self,
        account: &str,
        continue_failed: bool,
    ) -> Result<Option<Selected>, AccountChoiceRefusal> {
        let resubmit = if continue_failed {
            let turn = self.failed_root_turn().await?;
            let ids = ContinueIds::of(&turn.turn_id, account);
            if self
                .role
                .config
                .session
                .pending_queue
                .iter()
                .any(|item| item.id == ids.item_id)
            {
                return Err(AccountChoiceRefusal::AlreadyContinued);
            }
            Some((ids, continue_text(&turn)))
        } else {
            None
        };
        // A continue whose rebind already published (an earlier attempt that
        // stopped before its root-start) is this line head: it does not
        // rebind again.
        let published = resubmit.as_ref().is_some_and(|(ids, _)| {
            self.role
                .config
                .session
                .source_workflow_operation_id
                .as_deref()
                == Some(ids.operation_id.as_str())
        });
        if !published {
            let operation = resubmit.as_ref().map_or_else(
                || format!("plak:rebind:{}", uuid::Uuid::new_v4()),
                |(ids, _)| ids.operation_id.clone(),
            );
            self.rebind_account(account, &operation).await?;
        }
        let Some((ids, text)) = resubmit else {
            return Ok(None);
        };
        self.submit_continue(&ids, text).map(Some)
    }

    async fn rebind_account(
        &mut self,
        account: &str,
        operation: &str,
    ) -> Result<(), AccountChoiceRefusal> {
        self.role.idle_entry_pass_due = true;
        self.role.last_tree_revision.child_live.close_agent_view();
        let _ = self.role.events.send(ContentEvent::SetBusy(true));
        let _ = self.role.events.send(ContentEvent::Status {
            phase: HostPhase::Rebind,
            queue_len: self.role.config.session.pending_queue.len(),
            detail: None,
        });
        let rebound = Box::pin(handle_account_rebind(
            InterruptWatch::new(self.role.actions, &self.role.interrupt_state),
            &mut self.role.config,
            &mut self.opened,
            &mut self.role.stderr_buf,
            &self.role.events,
            account,
            operation,
        ))
        .await;
        let _ = self.role.events.send(ContentEvent::SetBusy(false));
        let _ = self.role.events.send(ContentEvent::Status {
            phase: HostPhase::Idle,
            queue_len: self.role.config.session.pending_queue.len(),
            detail: Some(HostStatusDetail::Ready {
                model_id: self.role.config.model_id.clone(),
                pause: plakous_host_protocol::status::HostPause::default(),
            }),
        });
        match rebound {
            Ok(Rebound::Switched | Rebound::Succeeded | Rebound::Unchanged) => Ok(()),
            Err(error) => Err(AccountChoiceRefusal::Failed(error.host_failure())),
        }
    }

    /// Queues the continue's root-start under its derived item id through
    /// the same record write as a typed prompt; the loop then drives it, and
    /// the ledger records its submission at dispatch.
    fn submit_continue(
        &mut self,
        ids: &ContinueIds,
        text: String,
    ) -> Result<Selected, AccountChoiceRefusal> {
        let mut queued = None;
        let committed = commit_session_update(
            &*self.role.config.store,
            &self.role.config.home,
            &mut self.role.config.session,
            |candidate| match candidate.enqueue_live_as(text, &ids.item_id) {
                Ok(item) => {
                    queued = Some(item);
                    true
                }
                Err(_) => false,
            },
        );
        let item = match (committed, queued) {
            (Ok(true), Some(item)) => item,
            (Ok(_), _) => {
                return Err(AccountChoiceRefusal::Failed(
                    CliFailure::new("the continue prompt was refused by the queue").host_failure(),
                ));
            }
            (Err(error), _) => {
                return Err(AccountChoiceRefusal::Failed(
                    CliFailure::new(format!("the continue prompt could not be queued: {error}"))
                        .host_failure(),
                ));
            }
        };
        let capacity = self.role.config.store.storage_admission();
        let echo = !matches!(capacity, crate::session_store::StorageAdmission::Refused(_));
        self.role.pending_storage_admission = Some(capacity);
        self.role.echoed_prompt = echo.then(|| item.id.clone());
        let identity = item.protocol(false);
        let _ = self
            .role
            .events
            .send(ContentEvent::PromptAdmitted(identity.clone()));
        if echo {
            let _ = self
                .role
                .events
                .send(ContentEvent::PromptStarted(Some(identity)));
            let _ = self.role.events.send(ContentEvent::UserMessage {
                text: item.text.clone(),
                sent_at_ms: Some(item.submitted_at_ms),
            });
        }
        Ok((item.text.clone(), Some(item), echo))
    }
}

fn refusal_note(refusal: &AccountChoiceRefusal) -> String {
    match refusal {
        AccountChoiceRefusal::RootBusy => {
            "root_busy: the root is running a turn; choose its account when it is idle".to_owned()
        }
        AccountChoiceRefusal::NoFailedTurn => {
            "no_failed_turn: the session's last root turn did not fail; nothing to continue"
                .to_owned()
        }
        AccountChoiceRefusal::AlreadyContinued => {
            "already_continued: that continue is already queued or submitted".to_owned()
        }
        AccountChoiceRefusal::Failed(failure) => {
            format!("account choice refused: {}", failure.diagnostic.as_str())
        }
    }
}

#[cfg(test)]
mod tests;
