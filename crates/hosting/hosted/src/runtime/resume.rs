//! The hosted half of what used to live in `runtime/settings.rs`: the resume
//! picker's projection and the idle-only refusal.
//!
//! Neither reads the product's configuration. `emit_resume_choices` turns
//! `SessionRecord`s the store already handed over into the protocol's own
//! `ResumeSessionEntry` rows, and `note_idle_only` is the loop's own refusal
//! vocabulary. Settings policy belongs to the product binary.

use crate::binding::TargetSelection;
use crate::sessions::SessionRecord;

use super::clear::exact_resume_target_matches;
use super::{HostConfig, HostEvent, MAX_RESUME_CHOICES, Sender};
use crate::sessions::{NoticeScope, TranscriptRow};

pub(super) fn emit_resume_choices<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    sessions: Vec<SessionRecord>,
    in_place: impl Fn(&SessionRecord) -> bool,
) {
    let total = sessions.len();
    let choices = sessions
        .into_iter()
        .take(MAX_RESUME_CHOICES)
        .map(|session| {
            let in_place = in_place(&session);
            plakous_host_protocol::views::ResumeSessionEntry {
                id: session.id,
                account: session.account,
                model: session.model,
                effort: session.effort,
                extra: (session.target_selection == TargetSelection::NativePriority)
                    .then(|| "Fast".to_owned()),
                title: session.title.filter(|title| !title.trim().is_empty()),
                // The store bounds the projection; the picker clips it again to
                // the row it actually has.
                first_prompt: session.first_prompt,
                created_at_unix: session.created_at_unix,
                updated_at_unix: session.updated_at_unix,
                in_place,
            }
        })
        .collect();
    let _ = events.send(HostEvent::ResumeSessions(choices));
    if total > MAX_RESUME_CHOICES {
        let _ = events.notice(TranscriptRow::info(NoticeScope::Session, format!(
            "resume picker shows the {MAX_RESUME_CHOICES} most recent cwd sessions; `/resume <uuid>` accepts any exact id"
        )));
    }
}

pub(super) fn handle_resume_picker<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    events: &Sender<P>,
) {
    match config.store.list_for_workspace(&config.cwd) {
        Ok(sessions) => {
            emit_resume_choices(events, sessions, |candidate| {
                exact_resume_target_matches(&config.session, candidate, &config.model_id)
            });
        }
        Err(err) => {
            let _ = events.notice(TranscriptRow::info(
                NoticeScope::Session,
                format!("resume: {err}"),
            ));
        }
    }
}

pub fn note_idle_only<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    action: &str,
) {
    let _ = events.notice(TranscriptRow::info(
        NoticeScope::Session,
        format!("{action}: available when idle (finish or interrupt the turn)"),
    ));
}
