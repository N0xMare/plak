//! Editable queue mutation, drain admission, and prompt history.

use crate::session_home::SessionHome;
use crate::session_log::{self, SessionEvent};
use crate::session_store::SessionRecordStore;
use crate::sessions::{
    QueueDrainState as SessionQueueDrainState, QueueItem as SessionQueueItem, SessionRecord,
};

use super::rebind::durable_tree_id;
use super::{
    HostEvent, MAX_PENDING_PROMPTS, MAX_PROMPT_BYTES, Sender, TopologyRows, UserAction, queue,
    topology_state,
};
use crate::sessions::{NoticeScope, TranscriptRow};

pub(super) fn emit_queue_snapshot<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    session: &SessionRecord,
) {
    let items = session
        .pending_queue
        .iter()
        .map(|item| item.protocol(session.queue_commands.is_rejected(item)))
        .collect();
    let drain = match session.queue_drain_state {
        SessionQueueDrainState::LiveAutoDrain => {
            plakous_host_protocol::QueueDrainState::LiveAutoDrain
        }
        SessionQueueDrainState::RestoredPaused => {
            plakous_host_protocol::QueueDrainState::RestoredPaused
        }
    };
    let _ = events.send(HostEvent::QueueSnapshot { items, drain });
}

/// A `restored_paused` tree used to be
/// a silent trap. Every `spawn_agent` failed and the queue stopped draining,
/// and the transcript said nothing about either. Attach now names both states
/// and the exact keystrokes that clear them, and the HUD carries
/// the pause so it is visible without opening a panel.
pub(super) fn restored_paused_status_suffix(
    rows: &dyn TopologyRows,
    session: &SessionRecord,
) -> Option<String> {
    let tree_paused =
        topology_state::observed_restored_paused(rows, &durable_tree_id(rows, session));
    let queue_paused = queue_drain_is_paused(session);
    match (tree_paused, queue_paused) {
        (false, false) => None,
        (true, true) => Some("agent tree paused · queue not draining · /tree then r".to_owned()),
        (true, false) => Some("agent tree paused · /tree then r".to_owned()),
        (false, true) => Some("queue paused, not draining · Tab then r".to_owned()),
    }
}

/// Admission pauses a *crash-restored queue*: with nothing pending there is
/// nothing paused, so a childless reopen says nothing about the queue.
pub(super) fn queue_drain_is_paused(session: &SessionRecord) -> bool {
    session.queue_drain_state == SessionQueueDrainState::RestoredPaused
        && !session.pending_queue.is_empty()
}

/// The attach-time note. Distinct from the status suffix because the note has
/// room for the whole remedy and the HUD does not. Every attach
/// re-announces it, so it is live only (spec 02 §5.4).
///
/// An interactive face already shows a paused tree as its one persisted
/// `agents paused` row (see `host_loop::pending`), so `tree_in_row` leaves the
/// tree out of the note; a JSONL face keeps the whole remedy here.
pub(super) fn emit_restored_paused_note<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    rows: &dyn TopologyRows,
    session: &SessionRecord,
    tree_in_row: bool,
) {
    let tree_paused = !tree_in_row
        && topology_state::observed_restored_paused(rows, &durable_tree_id(rows, session));
    let queue_paused = queue_drain_is_paused(session);
    if !tree_paused && !queue_paused {
        return;
    }
    let mut note = String::new();
    if tree_paused {
        note.push_str(
            "the agent tree is paused after an interrupt: spawning a subagent will be \
             refused until you resume it. Open /tree and press r to resume the tree, or \
             relaunch with `plak run --resume-tree`",
        );
    }
    if queue_paused {
        if !note.is_empty() {
            note.push_str(". ");
        }
        note.push_str(
            "the queue is paused and will not auto-drain: press Tab to focus the queue, \
             then r to resume delivery or Enter to send one item now; in plak agent, send a resume_queue command",
        );
    }
    let _ = events.send(HostEvent::Row(TranscriptRow::warning(
        NoticeScope::Queue,
        note,
    )));
}

pub(super) fn emit_prompt_history<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    current: &SessionRecord,
) {
    let _ = events.send(HostEvent::PromptHistorySnapshot(
        current.prompt_history.clone(),
    ));
}

/// Idle Prompt that will drive immediately must not project a one-item queue
/// chip. Remaining FIFO items are real chrome. UserMessage is emitted here so
/// hosted TUI shows the line before compaction / Plakous admit.
pub(super) fn admit_idle_immediate_prompt<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    session: &SessionRecord,
    action: UserAction<P>,
    item: SessionQueueItem,
) -> (String, Option<SessionQueueItem>, bool) {
    if session.pending_queue.len() > 1 {
        emit_queue_snapshot(events, session);
    }
    emit_prompt_history(events, session);
    accept_draft(events, action);
    let identity = item.protocol(false);
    let _ = events.send(HostEvent::PromptAdmitted(identity.clone()));
    let _ = events.send(HostEvent::PromptStarted(Some(identity)));
    let _ = events.send(HostEvent::UserMessage {
        text: crate::sessions::with_image_chips(
            &item.text,
            item.images.iter().map(crate::sessions::draft_image_chip),
        ),
        sent_at_ms: Some(item.submitted_at_ms),
    });
    (item.text.clone(), Some(item), true)
}

pub(super) fn reject_stale_queue_action<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    session: &SessionRecord,
) {
    emit_queue_snapshot(events, session);
    let _ = events.notice(TranscriptRow::warning(
        NoticeScope::Queue,
        "queue changed; refreshed from host snapshot",
    ));
}

pub(super) fn commit_session_update(
    store: &dyn SessionRecordStore,
    home: &SessionHome,
    session: &mut SessionRecord,
    update: impl FnOnce(&mut SessionRecord) -> bool,
) -> Result<bool, String> {
    let mut candidate = session.clone();
    if !update(&mut candidate) {
        return Ok(false);
    }
    candidate.touch();
    store.save(&mut candidate)?;
    if session.store_revision == 0 {
        session_log::emit(
            home,
            &candidate.id,
            SessionEvent::new("session.created")
                .str("id", &candidate.id)
                .opt_str("account", candidate.account.as_deref())
                .opt_str("model", candidate.model.as_deref())
                .str("workspace", candidate.workspace_root.display().to_string()),
        );
    }
    *session = candidate;
    Ok(true)
}

/// Closed diagnostic vocabulary: never derive action labels from prompt text.
#[derive(Clone, Copy)]
pub(super) enum QueuePersistAction {
    Admission,
    Drop,
    Delete,
    Edit,
    Move,
    ResumeDrain,
    PauseDrain,
    SendNow,
    RootDelivery,
}

impl QueuePersistAction {
    fn label(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::Drop => "drop",
            Self::Delete => "delete",
            Self::Edit => "edit",
            Self::Move => "move",
            Self::ResumeDrain => "resume_drain",
            Self::PauseDrain => "pause_drain",
            Self::SendNow => "send_now",
            Self::RootDelivery => "root_delivery",
        }
    }
}

// The store port returns strings. Only exact known refusal messages may be
// classified; arbitrary backend errors (including paths and SQL) stay opaque.
fn queue_store_failure_class(error: &str) -> &'static str {
    match error {
        "product session changed concurrently"
        | "transcript projection changed concurrently"
        | "product prompt history changed concurrently" => "concurrent_change",
        "product session revision is exhausted" => "revision_exhausted",
        "product session identity already exists or is invalid" => "identity_conflict",
        "queue identity contradicts durable product state"
        | "queue tombstone contradicts durable state" => "queue_identity_conflict",
        "product store is busy" => "store_busy",
        "another delivery intent is unresolved" => "delivery_in_flight",
        "could not configure product store durability"
        | "product store durability configuration was not retained"
        | "could not durably commit product session"
        | "could not durably prepare delivery" => "durability_failure",
        "could not append transcript projection"
        | "could not update transcript projection"
        | "could not prune transcript projection"
        | "could not mark transcript projection truncation"
        | "could not rewrite transcript projection"
        | "could not commit product transcript projection" => "transcript_rejection",
        "session has invalid product identity"
        | "session has non-canonical product identity"
        | "session has invalid pending queue state"
        | "pending queue exceeds the product count bound"
        | "pending queue byte count overflowed"
        | "prompt history exceeds the product count bound"
        | "prompt history contains adjacent duplicates"
        | "prompt history entry exceeds the product prompt bound"
        | "prompt history exceeds the product byte bound"
        | "delivery has invalid command identity"
        | "session has invalid delivery intent identity"
        | "delivery receipt history exceeds the operational bound"
        | "delivery intent contradicts canonical command"
        | "delivery intent contradicts pending queue content"
        | "unresolved delivery intent has no exact queue item"
        | "accepted delivery intent still has its exact queue item"
        | "session has multiple unresolved delivery intents"
        | "send-now delivery has invalid target turn"
        | "delivery queue item identity is not pending"
        | "new delivery queue item identity is not pending"
        | "delivery identity is already bound to different content or outcome"
        | "queue auto-drain cannot be live during a session workflow" => "validation_failure",
        _ => "store_failure",
    }
}

pub(super) fn report_queue_persist_failed<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    home: &SessionHome,
    session: &SessionRecord,
    action: QueuePersistAction,
    error: &str,
) {
    log_queue_persist_failed(home, session, action, error);
    queue_persist_failed(events, session);
}

pub(super) fn log_queue_persist_failed(
    home: &SessionHome,
    session: &SessionRecord,
    action: QueuePersistAction,
    error: &str,
) {
    session_log::emit(
        home,
        &session.id,
        SessionEvent::new("queue.persist_failed")
            .str("action", action.label())
            .str("failure_class", queue_store_failure_class(error))
            .u64("attempted_store_revision", session.store_revision),
    );
}

pub(super) fn queue_persist_failed<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    session: &SessionRecord,
) {
    emit_queue_snapshot(events, session);
    let _ = events.notice(TranscriptRow::error(
        NoticeScope::Queue,
        "queue change was not saved; authoritative queue is unchanged",
    ));
}

pub(super) fn queue_refusal<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    action: &str,
    refusal: queue::Refusal,
) {
    let reason = match refusal {
        queue::Refusal::Empty => "the message has no text and no image".to_owned(),
        queue::Refusal::TooLarge => format!(
            "text exceeds the {} MiB product prompt bound",
            MAX_PROMPT_BYTES / (1024 * 1024)
        ),
        queue::Refusal::Full => {
            format!("queue already has {MAX_PENDING_PROMPTS} editable prompts")
        }
        queue::Refusal::Stale => "queue item identity/revision is stale or locked".to_owned(),
        queue::Refusal::TooManyImages => format!(
            "a prompt carries at most {} images",
            plakous::agent::MAX_USER_IMAGES
        ),
        queue::Refusal::ImageTooLarge => {
            "an image exceeds 3.75 MiB; downscale it and attach it again".to_owned()
        }
        queue::Refusal::NotAnImage => {
            "an attachment is not a png, jpeg, gif or webp image".to_owned()
        }
        queue::Refusal::MessageTooLarge(error) => error.to_string(),
        queue::Refusal::ImagesFull => format!(
            "queued prompts already hold {} MiB of images",
            crate::sessions::MAX_PENDING_QUEUE_IMAGE_BYTES / (1024 * 1024)
        ),
    };
    let _ = events.notice(TranscriptRow::error(
        NoticeScope::Queue,
        format!("{action} refused: {reason}"),
    ));
}

pub(super) fn accept_draft<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    action: UserAction<P>,
) {
    let _ = events.send(HostEvent::DraftAccepted(action));
}

pub(super) fn reject_draft<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    action: UserAction<P>,
) {
    let _ = events.send(HostEvent::DraftRejected(action));
}

pub(super) fn recovery_blocked_action<P: plakous_host_protocol::ProtocolExtension>(
    action: &UserAction<P>,
) -> bool {
    matches!(
        action,
        UserAction::Prompt(_)
            | UserAction::FollowUp(_)
            | UserAction::Steer(_)
            | UserAction::DropQueued
            | UserAction::SendQueuedNow { .. }
            | UserAction::DeleteQueued { .. }
            | UserAction::EditQueued { .. }
            | UserAction::MoveQueued { .. }
            | UserAction::ResumeQueueDrain
            | UserAction::Compact
            | UserAction::ForkSession { .. }
            | UserAction::RetryAttach
            | UserAction::InvokeTool { .. }
    )
}

/// The action that asked for recovery could not run because the reopen
/// failed. It is refused, and its draft handed back, with the reason; the next
/// action tries the reopen again.
pub(super) fn refuse_recovery_blocked_action<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    action: &UserAction<P>,
    failure: &crate::CliFailure,
) {
    let _ = events.notice(TranscriptRow::error(
        NoticeScope::Queue,
        failure.message().to_owned(),
    ));
    if matches!(
        action,
        UserAction::Prompt(_) | UserAction::FollowUp(_) | UserAction::Steer(_)
    ) {
        reject_draft(events, action.clone());
    }
}

/// The detached shell has no session to reopen in place; `/attach` does it.
pub(super) fn refuse_detached_recovery_action<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    action: &UserAction<P>,
) {
    let _ = events.notice(TranscriptRow::error(
        NoticeScope::Queue,
        "this session is not attached; /attach reopens it, which recovers any interrupted turn",
    ));
    if matches!(
        action,
        UserAction::Prompt(_) | UserAction::FollowUp(_) | UserAction::Steer(_)
    ) {
        reject_draft(events, action.clone());
    }
}

pub(super) fn queue_edit_applied<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    id: String,
) {
    let _ = events.send(HostEvent::QueueEditApplied { id });
}

pub(super) fn queue_edit_refused<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    id: String,
) {
    let _ = events.send(HostEvent::QueueEditRefused { id });
}

/// Clear the presentation meter when no descriptor-bound Plakous status exists.
pub(super) fn emit_unavailable_context<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
) {
    let _ = events.send(HostEvent::Usage(
        plakous_host_protocol::views::presentation::ContextUsage {
            breakdown: None,
            prepared_input: None,
            total_window: None,
            admitted_ceiling: None,
            gauge_percent: None,
            display_percent: None,
            ceiling_excess: None,
            evidence: "unavailable".into(),
            non_guarantee: None,
            status: "unavailable".into(),
        },
    ));
}

#[cfg(test)]
mod tests;
