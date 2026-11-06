//! Idle interactive continuation uses ordinary root admission and its journal.

use super::{CliFailure, CliResult, HostStep, Instant, RootActor, SessionQueueItem};
use crate::driver::wake::{WakeCoalescer, WakeDecision, WakeSources};
use crate::sessions::{NoticeScope, TranscriptRow};
use plakous_host_protocol::status::{HostPhase, HostStatusDetail};
use std::time::Duration;

#[cfg(test)]
pub(in crate::runtime) const QUIET_WINDOW: Duration =
    crate::driver::wake::WakePolicy::Root.window();
/// Consecutive transient store refusals a wake retries on a doubling delay,
/// starting at [`TRANSIENT_FIRST_DELAY`], before it reports itself paused.
const TRANSIENT_RETRIES: u32 = 5;
const TRANSIENT_FIRST_DELAY: Duration = Duration::from_secs(1);
/// A wake paused by transient refusals is retried at this interval, so the
/// pause ends on its own once the store answers again.
const TRANSIENT_REARM: Duration = Duration::from_secs(60);
pub(in crate::runtime) const WAKE_PROMPT: &str = "Background processes finished or sub-agent results arrived while you were idle; continue the current task, or reply briefly if nothing remains.";

#[derive(Default)]
pub(super) struct RootWake {
    session_id: String,
    coalescer: WakeCoalescer,
    suppression_pending: bool,
    pause_pending: Option<String>,
    retry: Option<TransientRetry>,
    pub(super) prepared_context: Option<crate::context::ContextProjection>,
}

/// A wake whose reads the session store refused transiently. It is neither
/// suppressed nor parked: the observed batch stays, and the next attempt
/// waits for `next_at`.
struct TransientRetry {
    session_id: String,
    failures: u32,
    next_at: Instant,
}

impl RootWake {
    fn observe(&mut self, session_id: &str, ids: Vec<String>, now: Instant) -> bool {
        if self.session_id != session_id {
            *self = Self {
                session_id: session_id.to_owned(),
                ..Self::default()
            };
        }
        matches!(
            self.coalescer.observe(
                &WakeSources {
                    running: Vec::new(),
                    arrivals: ids.into_iter().collect(),
                    children: Vec::new(),
                    claimed: Vec::new(),
                },
                now
            ),
            WakeDecision::Fire(_)
        )
    }

    fn park(&mut self) {
        self.coalescer.park();
    }

    fn retry_waiting(&self, session_id: &str, now: Instant) -> bool {
        self.retry
            .as_ref()
            .is_some_and(|retry| retry.session_id == session_id && now < retry.next_at)
    }

    /// Records one more transient refusal and returns the consecutive count
    /// and the delay before the next attempt.
    fn defer(&mut self, session_id: &str, now: Instant) -> (u32, Duration) {
        let failures = self
            .retry
            .as_ref()
            .filter(|retry| retry.session_id == session_id)
            .map_or(0, |retry| retry.failures)
            .saturating_add(1);
        let delay = if failures <= TRANSIENT_RETRIES {
            TRANSIENT_FIRST_DELAY * 2u32.pow(failures - 1)
        } else {
            TRANSIENT_REARM
        };
        self.retry = Some(TransientRetry {
            session_id: session_id.to_owned(),
            failures,
            next_at: now + delay,
        });
        (failures, delay)
    }
}

/// Whether child wakes are already suppressed in the session's intents.
pub(in crate::runtime) fn wake_suppression_durable(
    store: &dyn crate::session_store::SessionRecordStore,
    session_id: &str,
) -> bool {
    let Ok(intents) = store.load_intents(session_id) else {
        return false;
    };
    let Ok(views) = store.load_views(session_id) else {
        return false;
    };
    views
        .unwrap_or_else(crate::sessions::projector::SessionViews::empty)
        .wake_suppressed(&intents)
}

pub(in crate::runtime) fn is_wake_delivery(
    session: &crate::sessions::SessionRecord,
    item: &SessionQueueItem,
) -> bool {
    session
        .queue_commands
        .pending_for(item)
        .is_some_and(|submission| {
            submission.kind == crate::sessions::projector::SubmissionKind::HostWake
        })
}

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    pub(super) fn suppress_child_wake(&mut self, reason: Option<String>) -> CliResult<()> {
        // A repeated stop records nothing new: the suppression row is
        // already the durable fact.
        if reason.is_none()
            && !self.role.child_wake.suppression_pending
            && self.role.config.session.store_revision != 0
            && wake_suppression_durable(
                self.role.config.store.as_ref(),
                &self.role.config.session.id,
            )
        {
            self.role.config.session.child_wake_suppressed = true;
            return Ok(());
        }
        self.role.config.session.child_wake_suppressed = true;
        self.role.child_wake.suppression_pending = true;
        self.role.child_wake.pause_pending = reason.clone();
        if self.role.storage_interrupted && self.role.config.store.storage_unavailable() {
            return Ok(());
        }
        if self.role.config.session.store_revision == 0 {
            self.role
                .config
                .store
                .save(&mut self.role.config.session)
                .map_err(CliFailure::from)?;
        }
        let pause =
            reason.map(
                |reason| crate::sessions::projector::HostPauseCause::WakePreflightFailed { reason },
            );
        self.role
            .config
            .store
            .write_host_controls(&self.role.config.session.id, true, pause.clone())
            .map_err(CliFailure::from)?;
        self.role.child_wake.suppression_pending = false;
        self.role.child_wake.pause_pending = None;
        if pause.is_some() {
            self.role.config.session.queue_drain_state =
                super::SessionQueueDrainState::RestoredPaused;
        }
        Ok(())
    }

    pub(super) fn pause_unrecorded(&mut self, reason: String) -> CliResult<()> {
        self.pause_queue(reason, false)?;
        let _ = self
            .role
            .events
            .send(crate::surface_protocol::ContentEvent::Status {
                phase: HostPhase::Idle,
                queue_len: self.role.config.session.pending_queue.len(),
                detail: Some(HostStatusDetail::PromptNotAdmittedQueuePaused),
            });
        Ok(())
    }

    pub(super) fn pause_queue(&mut self, reason: String, suppress_wakes: bool) -> CliResult<()> {
        if suppress_wakes {
            self.role.config.session.child_wake_suppressed = true;
            self.role.child_wake.park();
        }
        self.role.config.session.queue_drain_state = super::SessionQueueDrainState::RestoredPaused;
        if self.role.storage_interrupted && self.role.config.store.storage_unavailable() {
            self.role.config.session.queue_drain_state =
                super::SessionQueueDrainState::RestoredPaused;
            return Ok(());
        }
        if self.role.config.session.store_revision == 0 {
            self.role
                .config
                .store
                .save(&mut self.role.config.session)
                .map_err(CliFailure::from)?;
        }
        self.role
            .config
            .store
            .write_host_controls(
                &self.role.config.session.id,
                suppress_wakes,
                Some(crate::sessions::projector::HostPauseCause::UnrecordedRefusal { reason }),
            )
            .map_err(CliFailure::from)?;
        self.role.config.session.queue_drain_state = super::SessionQueueDrainState::RestoredPaused;
        Ok(())
    }

    pub(in crate::runtime) async fn child_wake_prompt(
        &mut self,
        now: Instant,
    ) -> CliResult<HostStep> {
        if self.role.config.interactive
            && let Some(surface) = self.role.config.surface.as_deref()
            && let Some(enabled) = surface.child_wake_enabled()
        {
            self.role.config.auto_continue = enabled;
        }
        if self.role.child_wake.suppression_pending
            && self
                .suppress_child_wake(self.role.child_wake.pause_pending.clone())
                .is_err()
        {
            return Ok(HostStep::Continue);
        }
        if self
            .role
            .child_wake
            .retry_waiting(&self.role.config.session.id, now)
        {
            return self.wake_deferred();
        }
        let suppressed = self.role.config.session.child_wake_suppressed;
        let step = match self.try_child_wake_prompt(now).await {
            Ok(step) => {
                self.role.child_wake.retry = None;
                Ok(step)
            }
            Err(error) => {
                if let Some(refusal) = error.store_refusal() {
                    self.defer_child_wake(refusal, now);
                    return self.wake_deferred();
                }
                tracing::warn!(
                    target: "plak.wake",
                    cause = "refused",
                    "sub-agent auto-continue paused until the next prompt"
                );
                self.role.child_wake.retry = None;
                let _ = self.role.events.notice(
                    TranscriptRow::error(
                        NoticeScope::Tree,
                        format!(
                            "Sub-agent auto-continue paused until the next prompt: {}",
                            error.message()
                        ),
                    )
                    .with_optional_remedy(error.remedy()),
                );
                let _ = self.suppress_child_wake(Some(error.message().to_owned()));
                self.role.child_wake.park();
                Ok(HostStep::Continue)
            }
        };
        if !suppressed && self.role.config.session.child_wake_suppressed {
            self.auto_continue_pause_reported();
        }
        self.publish_pending_continuation();
        step
    }

    /// Posts the host-authored continuation this host was started with, as a
    /// root host wake: the same journaled submission, crash and reopen rules
    /// as an idle wake. It waits while another submission is unresolved.
    pub(super) async fn continuation_prompt(&mut self) -> CliResult<HostStep> {
        if self.role.recovery.blocked()
            || self.role.pending_start.is_some()
            || self.role.config.session.queue_commands.has_pending()
        {
            return Ok(HostStep::Continue);
        }
        let Some(text) = self.role.config.continuation.take() else {
            return Ok(HostStep::Continue);
        };
        // The same context preflight a wake runs, which a host-authored
        // prompt's admission reads back.
        match self.fit_child_wake(&text).await {
            Ok(WakeFit::Fits) => {}
            Ok(WakeFit::Stopped(_)) => {
                self.role.config.continuation = Some(text);
                return Ok(HostStep::Continue);
            }
            Ok(WakeFit::DoesNotFit) | Err(_) => {
                self.role.child_wake.prepared_context = None;
                let _ = self.role.events.notice(TranscriptRow::warning(
                    NoticeScope::Session,
                    "the reload continuation could not be prepared; send a prompt to continue",
                ));
                return Ok(HostStep::Continue);
            }
        }
        let mut candidate = self.role.config.session.clone();
        let item = candidate
            .enqueue_live(text.as_str())
            .map_err(|_| CliFailure::new("the continuation was refused by the queue"))?;
        self.role.config.session = super::super::result_mail::prepare_instruction(
            &self.role.config.topology_rows(),
            self.role.config.store.as_ref(),
            &candidate,
            &item,
            &text,
            None,
            &["host-continuation".to_owned()],
        )
        .await
        .map_err(CliFailure::from)?;
        Ok(HostStep::Prompt {
            text: item.text.clone(),
            queued_delivery: Some(item),
            user_echoed: true,
        })
    }

    fn wake_deferred(&mut self) -> CliResult<HostStep> {
        self.publish_pending_continuation();
        Ok(HostStep::Continue)
    }

    /// A transient store refusal neither suppresses nor parks the wake: it is
    /// retried on a doubling delay, then every [`TRANSIENT_REARM`] once the
    /// retries are spent, which is when the pause is reported.
    fn defer_child_wake(&mut self, refusal: crate::error::StoreRefusal, now: Instant) {
        let (failures, delay) = self
            .role
            .child_wake
            .defer(&self.role.config.session.id, now);
        let storage_low = refusal == crate::error::StoreRefusal::LowSpace;
        let message = if storage_low {
            "sub-agent auto-continue waiting: product storage is below the admission floor"
        } else {
            "the session store refused a sub-agent wake read; retrying"
        };
        tracing::warn!(
            target: "plak.wake",
            cause = refusal.as_str(),
            failures,
            retry_in_secs = delay.as_secs(),
            "{message}"
        );
        if failures == TRANSIENT_RETRIES + 1 {
            let _ = self.role.events.notice(TranscriptRow::warning(
                NoticeScope::Tree,
                if storage_low {
                    format!("Sub-agent auto-continue waiting: product storage is below the admission floor; rechecks every {} s", TRANSIENT_REARM.as_secs())
                } else { format!(
                    "Sub-agent auto-continue paused: the session store is {}; it retries every {} s",
                    refusal.as_str(),
                    TRANSIENT_REARM.as_secs()
                ) },
            ));
        }
    }

    async fn try_child_wake_prompt(&mut self, now: Instant) -> CliResult<HostStep> {
        let jobs = self
            .opened
            .agent()
            .map_or_else(Vec::new, plakous::agent::Agent::jobs);
        self.try_child_wake_with_jobs(now, jobs).await
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Child wake admission reconciles unread mail, joined jobs, suppression, and durable wake preparation before dispatch."
    )]
    async fn try_child_wake_with_jobs(
        &mut self,
        now: Instant,
        jobs: Vec<plakous::tools::JobView>,
    ) -> CliResult<HostStep> {
        let views = self
            .role
            .config
            .store
            .load_views(&self.role.config.session.id)
            .map_err(CliFailure::new)?
            .unwrap_or_else(crate::sessions::projector::SessionViews::empty);
        let intents = self
            .role
            .config
            .store
            .load_intents(&self.role.config.session.id)
            .map_err(CliFailure::new)?;
        // Suppression is the refusal. A later root turn's admission clears
        // it, so a child result after that prompt wakes the root again.
        self.role.config.session.child_wake_suppressed = views.wake_suppressed(&intents);
        if views.pause_cause(&intents).is_some() {
            self.role.child_wake.park();
            return Ok(HostStep::Continue);
        }
        self.role
            .config
            .session
            .pending_queue
            .retain(|item| !self.role.config.session.queue_commands.is_consumed(item));
        let session = &self.role.config.session;
        if !self.role.config.prompt_host_facts().host_wakes
            || self.role.recovery.blocked()
            || session.child_wake_suppressed
            || self.role.pending_start.is_some()
            || !session.pending_queue.is_empty()
            || session.queue_commands.has_pending()
        {
            self.role.child_wake.park();
            return Ok(HostStep::Continue);
        }
        let rows = self.role.config.topology_rows();
        let runtime = self.role.last_tree_revision.runtime(&rows, session);
        let notices: Vec<_> = jobs
            .into_iter()
            .filter(|job| job.status != plakous::tools::JobStatus::Running)
            .collect();
        let ids = wake_ids(runtime.as_deref(), &notices)?;
        if !self.role.child_wake.observe(&session.id, ids, now) {
            return Ok(HostStep::Continue);
        }
        // A wake is to be admitted: this is its admission pass to the tail,
        // which also settles acknowledgements and returns the batch reads.
        super::super::projection::root_tree_pass(
            self.opened.sessions(),
            &rows,
            &super::durable_tree_id(&rows, session),
            self.opened.locator().session_id(),
            super::super::projection::RootTreePass::Admission,
        )
        .await?;
        let ids = wake_ids(runtime.as_deref(), &notices)?;
        if ids.is_empty() {
            self.role.child_wake.park();
            return Ok(HostStep::Continue);
        }
        // Recheck ingress after observing mail: user input and shutdown outrank a wake.
        let watch = super::InterruptWatch::new(self.role.actions, &self.role.interrupt_state);
        watch.poll();
        if self.role.interrupt_state.borrow().has_deferred() || watch.interrupted() {
            return Ok(HostStep::Continue);
        }
        let admission = self.role.config.store.storage_admission();
        super::super::info::send_session_info_with_storage(
            &self.role.events,
            &self.role.config,
            &admission,
        );
        if matches!(
            admission,
            crate::session_store::StorageAdmission::Refused(_)
        ) {
            return Err(CliFailure::from(
                crate::session_store::RecordStoreError::refused(
                    crate::error::StoreRefusal::LowSpace,
                    "operator storage is below the admission floor",
                ),
            ));
        }
        let (command, batch) = super::super::result_mail::select(
            &rows,
            session,
            WAKE_PROMPT,
            super::super::result_mail::LossNotice::WithMail,
        )
        .map_err(CliFailure::new)?;
        if batch.is_none() && notices.is_empty() {
            self.role.child_wake.park();
            return Ok(HostStep::Continue);
        }
        // Compaction requires an idle journal; budget the complete mail batch
        // before preparing its immutable delivery receipt.
        if !self.preflight_child_wake(&command).await? {
            return Ok(HostStep::Continue);
        }
        let watch = super::InterruptWatch::new(self.role.actions, &self.role.interrupt_state);
        watch.poll();
        if self.role.interrupt_state.borrow().has_deferred() || watch.interrupted() {
            return Ok(HostStep::Continue);
        }
        let mut candidate = self.role.config.session.clone();
        let item = candidate
            .enqueue_live(WAKE_PROMPT)
            .map_err(|_| CliFailure::new("child wake queue admission was refused"))?;
        // Queue identity and the exact instruction batch publish atomically;
        // an unresolved wake cannot be edited or exported as a user prompt.
        self.role.config.session = super::super::result_mail::prepare_instruction(
            &self.role.config.topology_rows(),
            self.role.config.store.as_ref(),
            &candidate,
            &item,
            &command,
            batch,
            &ids,
        )
        .await
        .map_err(CliFailure::from)?;
        self.role.child_wake.park();
        Ok(HostStep::Prompt {
            text: item.text.clone(),
            queued_delivery: Some(item),
            user_echoed: true,
        })
    }

    async fn preflight_child_wake(&mut self, command: &str) -> CliResult<bool> {
        match self.fit_child_wake(command).await? {
            WakeFit::Fits => return Ok(true),
            WakeFit::Stopped(stopped) => {
                let _ = stopped;
                self.role.child_wake.park();
                return Ok(false);
            }
            WakeFit::DoesNotFit => {}
        }
        self.suppress_child_wake(Some(
            "Sub-agent auto-continue preflight did not fit".to_owned(),
        ))?;

        let _ = self.role.events.notice(TranscriptRow::warning(
            NoticeScope::Tree,
            "Sub-agent auto-continue preflight paused until the next prompt",
        ));
        self.role.child_wake.park();
        Ok(false)
    }

    pub(super) async fn fit_child_wake(&mut self, command: &str) -> CliResult<WakeFit> {
        let watch = super::InterruptWatch::new(self.role.actions, &self.role.interrupt_state);
        watch.begin();
        self.refresh_native_authority_before_prompt().await?;
        if let Some(stopped) = self.stop_preparation(&mut None)? {
            return Ok(WakeFit::Stopped(stopped));
        }
        let command = plakous::agent::UserInput::from(command.to_owned());
        let context = super::query_context(
            &self.opened,
            super::ContextQuery::WithPendingUserInput(command.clone()),
        )
        .await?;
        if let Some(stopped) = self.stop_preparation(&mut None)? {
            return Ok(WakeFit::Stopped(stopped));
        }
        if !context.compaction_verdict().is_due() {
            self.role.child_wake.prepared_context = Some(context);
            return Ok(WakeFit::Fits);
        }
        if !self.role.config.compaction_enabled {
            if context.compaction_verdict().overflow() {
                return Err(CliFailure::new(super::scheduled_compaction_disabled_note(
                    context.disposition(),
                )));
            }
            self.role.child_wake.prepared_context = Some(context);
            return Ok(WakeFit::Fits);
        }
        self.role.config.session.queue_drain_state = super::SessionQueueDrainState::LiveAutoDrain;
        super::observe_compact(&self.role.events, 0, true);
        let outcome = super::run_scheduled_compaction_until_fits(
            super::InterruptWatch::new(self.role.actions, &self.role.interrupt_state),
            &mut self.role.config,
            &self.opened,
            &mut self.role.stderr_buf,
            &self.role.events,
            self.role.log.as_ref(),
            &command,
            None,
        )
        .await;
        super::observe_compact(&self.role.events, 0, false);
        if let Some(stopped) = self.stop_preparation(&mut None)? {
            return Ok(WakeFit::Stopped(stopped));
        }
        // A wake that does not fit would be withheld by its turn after its
        // admission acknowledged the mail it carries; it waits instead. A
        // wake has no bare prompt to fall back to: its batch is the point.
        let sends = matches!(
            outcome?,
            super::ScheduledPrompt::Send {
                over_window: false,
                ..
            }
        );
        if sends {
            self.role.child_wake.prepared_context = Some(
                super::query_context(
                    &self.opened,
                    super::ContextQuery::WithPendingUserInput(command),
                )
                .await?,
            );
        }
        Ok(if sends {
            WakeFit::Fits
        } else {
            WakeFit::DoesNotFit
        })
    }
}

pub(super) enum WakeFit {
    Fits,
    DoesNotFit,
    Stopped(super::prompt::preparation::Stopped),
}

/// Whether a child result wakes `config`'s root across one quiet window,
/// with no new prompt. The actor is released afterwards.
#[cfg(test)]
pub(in crate::runtime) fn probe_child_wake(
    config: super::super::HostConfig<crate::runtime::test_kit::TestExtension>,
) -> bool {
    let (_actions, receiver) =
        crate::surface_protocol::user_action_channel::<crate::runtime::test_kit::TestExtension>();
    let (events, _observed) =
        crate::surface_protocol::host_event_channel::<crate::runtime::test_kit::TestExtension>();
    let mut host = RootActor::open_blocking(
        config,
        &receiver,
        crate::runtime::Sender::over(events),
        None,
    )
    .expect("probe host");
    let now = Instant::now();
    let _ = host
        .block(async |host| host.child_wake_prompt(now).await)
        .expect("first wake");
    let woke = matches!(
        host.block(async |host| host.child_wake_prompt(now + QUIET_WINDOW).await)
            .expect("second wake"),
        HostStep::Prompt { .. }
    );
    host.block(async |host| host.release().await)
        .expect("probe release");
    woke
}

#[cfg(test)]
mod tests;

/// The unread result and question mail and the finished jobs a wake would
/// carry.
fn wake_ids(
    runtime: Option<&crate::topology_runtime::ProductTopologyRuntime>,
    notices: &[plakous::tools::JobView],
) -> CliResult<Vec<String>> {
    let mails = if let Some(runtime) = runtime {
        runtime
            .read(|tree| {
                tree.pending_mail(&tree.root_agent_id)
                    .into_iter()
                    .filter(|mail| {
                        matches!(
                            mail.purpose,
                            crate::agent_tree_protocol::MessagePurpose::Result
                                | crate::agent_tree_protocol::MessagePurpose::Question
                        )
                    })
                    .map(|mail| mail.message_id.clone())
                    .collect::<Vec<_>>()
            })
            .map_err(|error| CliFailure::new(error.message))?
    } else {
        Vec::new()
    };
    Ok(mails
        .into_iter()
        .chain(notices.iter().map(|job| format!("p{}", job.number)))
        .collect())
}
