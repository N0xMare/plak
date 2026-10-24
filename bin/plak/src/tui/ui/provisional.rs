//! Provisional `sending…` rows (spec 02 §5.4): drafts the mailbox took that
//! the host has not yet answered with its own row. They are drawn from the
//! pending and accepted draft lists, never stored as transcript blocks, and
//! carry no queue number: the queue panel's `#N` rows stay the host's.

use super::{App, Frame, Line, Rect, Span, Style, UnicodeWidthStr, UserAction};
use crate::tui::blocks::PROMPT_PREFIX;
use plakous_host_protocol::intent::Draft;

/// The marker every provisional row ends with.
pub(super) const SENDING: &str = "sending…";

/// Rows of the transcript-tail shelf, at most.
const MAX_SHELF_ROWS: usize = 3;

/// A draft the host acknowledged, still drawn until the host's own row for
/// it lands.
pub(super) struct AcceptedDraft {
    action: UserAction,
    /// The queue item the host admitted it as, once `PromptAdmitted` named it.
    admitted: Option<String>,
    /// `PromptStarted` named that item: the next user row is its own.
    started: bool,
}

/// One provisional row's text: the draft on one line, with its image count.
fn preview(draft: &Draft) -> String {
    let text = draft.text.trim().replace('\n', " ");
    match draft.images.len() {
        0 => text,
        1 => format!("{text} [1 image]"),
        n => format!("{text} [{n} images]"),
    }
}

impl App {
    /// The host acknowledged the draft durably. Its row stays provisional
    /// until the host's own row lands, so a frame drawn between the two never
    /// loses the text.
    pub(super) fn accept_pending_draft(&mut self, action: &UserAction) {
        if let Some(action) = self.remove_pending_draft(action) {
            self.accepted_drafts.push_back(AcceptedDraft {
                action,
                admitted: None,
                started: false,
            });
        }
    }

    /// The host's live steer row landed for the oldest accepted steer.
    pub(super) fn retire_accepted_steer(&mut self) {
        if let Some(index) = self
            .accepted_drafts
            .iter()
            .position(|accepted| matches!(accepted.action, UserAction::Steer(_)))
        {
            self.accepted_drafts.remove(index);
        }
    }

    /// A user row landed: it is the prompt `PromptStarted` named, or else the
    /// oldest accepted prompt.
    pub(super) fn retire_started_draft(&mut self) {
        let index = self
            .accepted_drafts
            .iter()
            .position(|accepted| accepted.started)
            .or_else(|| {
                self.accepted_drafts
                    .iter()
                    .position(|accepted| !matches!(accepted.action, UserAction::Steer(_)))
            });
        if let Some(index) = index {
            self.accepted_drafts.remove(index);
        }
    }

    /// `PromptAdmitted` names the queue item of the oldest accepted prompt not
    /// yet named. The host publishes the queue snapshot that holds the item
    /// before that admission, and the mailbox keeps only the newest snapshot,
    /// so the queue already shows the item, or the next snapshot applied is
    /// at least that new: the row retires on whichever comes first, even if
    /// the item has left the queue by then.
    pub(super) fn note_admitted_draft(&mut self, id: String) {
        let shown = self.local_queue.iter().any(|item| item.id == id);
        let Some(index) = self.accepted_drafts.iter().position(|accepted| {
            accepted.admitted.is_none() && !matches!(accepted.action, UserAction::Steer(_))
        }) else {
            return;
        };
        if shown {
            self.accepted_drafts.remove(index);
        } else if let Some(accepted) = self.accepted_drafts.get_mut(index) {
            accepted.admitted = Some(id);
        }
    }

    /// `PromptStarted` names the item an idle prompt was admitted as.
    pub(super) fn note_started_draft(&mut self, id: &str) {
        if let Some(accepted) = self
            .accepted_drafts
            .iter_mut()
            .find(|accepted| accepted.admitted.as_deref() == Some(id))
        {
            accepted.started = true;
        }
    }

    /// A queue snapshot landed: every admitted draft's own row is in it, or
    /// was and has already left the queue.
    pub(super) fn retire_admitted_drafts(&mut self) {
        self.accepted_drafts
            .retain(|accepted| accepted.admitted.is_none() || accepted.started);
    }

    /// The mailbox lost a correlation receipt, perhaps the `PromptAdmitted`
    /// that would have named an accepted prompt; with no identity to wait
    /// on, those rows retire now rather than sit beside the host's.
    pub(super) fn retire_unnamed_drafts(&mut self) {
        self.accepted_drafts.retain(|accepted| {
            accepted.admitted.is_some() || matches!(accepted.action, UserAction::Steer(_))
        });
    }

    /// Every provisional draft, oldest first.
    fn provisional_drafts(&self) -> impl Iterator<Item = &UserAction> {
        self.accepted_drafts
            .iter()
            .map(|accepted| &accepted.action)
            .chain(&self.pending_drafts)
    }

    /// Idle prompts, drawn as dim user rows at the transcript tail.
    pub(super) fn provisional_prompts(&self) -> impl Iterator<Item = &Draft> {
        self.provisional_drafts().filter_map(|action| match action {
            UserAction::Prompt(draft) => Some(draft),
            _ => None,
        })
    }

    /// Follow-ups and steers, drawn as dim rows in the queue panel; `true`
    /// marks a steer.
    pub(super) fn provisional_queue_rows(&self) -> impl Iterator<Item = (bool, &Draft)> {
        self.provisional_drafts().filter_map(|action| match action {
            UserAction::FollowUp(draft) => Some((false, draft)),
            UserAction::Steer(draft) => Some((true, draft)),
            _ => None,
        })
    }

    /// Height of the transcript-tail shelf of provisional prompts.
    pub(super) fn sending_shelf_height(&self) -> u16 {
        u16::try_from(self.provisional_prompts().count().min(MAX_SHELF_ROWS)).unwrap_or(0)
    }

    /// The shelf directly under the transcript: each provisional prompt as
    /// the user band it will become, dimmed, with the `sending…` marker.
    pub(super) fn draw_sending_shelf(&self, frame: &mut Frame<'_>, area: Rect) {
        let (area, _track) = super::scrollbar::Scrollbar::split(area);
        let width = usize::from(area.width);
        let marker_width = UnicodeWidthStr::width(SENDING);
        let budget = width.saturating_sub(PROMPT_PREFIX.width() + marker_width + 2);
        let lines: Vec<Line<'static>> = self
            .provisional_prompts()
            .take(usize::from(area.height))
            .map(|draft| {
                let text = super::composer_chrome::clip_text(&preview(draft), budget);
                let used = PROMPT_PREFIX.width() + text.width() + marker_width + 1;
                Line::from(vec![
                    Span::styled(PROMPT_PREFIX, self.theme.chrome_dim),
                    Span::styled(text, self.theme.chrome_dim),
                    Span::raw(" ".repeat(width.saturating_sub(used).max(1))),
                    Span::styled(SENDING, self.theme.chrome_dim),
                ])
                .style(Style::default().bg(self.theme.user_bg))
            })
            .collect();
        frame.render_widget(ratatui::widgets::Paragraph::new(lines), area);
    }

    /// One provisional queue-panel row: ` · text · sending…`, or with
    /// `steer · ` before the text.
    pub(super) fn provisional_queue_line(
        &self,
        steer: bool,
        draft: &Draft,
        width: u16,
    ) -> Line<'static> {
        let lead = if steer { " steer · " } else { " · " };
        let tail = format!(" · {SENDING}");
        let budget = usize::from(width)
            .saturating_sub(lead.width() + tail.width())
            .min(40);
        let text = super::composer_chrome::clip_text(&preview(draft), budget);
        Line::from(Span::styled(
            format!("{lead}{text}{tail}"),
            self.theme.chrome_dim,
        ))
    }
}
