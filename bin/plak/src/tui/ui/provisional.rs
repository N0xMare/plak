//! Provisional `sending…` rows (spec 02 §5.4): drafts the mailbox took that
//! the host has not yet taken over with its own row, queue or steer chrome.
//! They are drawn from the
//! pending and accepted draft lists, never stored as transcript blocks, and
//! carry no queue number: the queue panel's `#N` rows stay the host's.

use super::{App, Frame, Line, Rect, Span, Style, UnicodeWidthStr, UserAction};
use crate::tui::blocks::PROMPT_PREFIX;
use plakous_host_protocol::intent::Draft;

/// The marker every provisional row ends with.
pub(super) const SENDING: &str = "sending…";

/// Rows of the transcript-tail shelf, at most.
const MAX_SHELF_ROWS: usize = 3;

/// How many of `total` rows fit in `slots`, and how many a trailing
/// `+N sending` line names when the rest do not.
pub(super) fn sending_overflow(total: usize, slots: usize) -> (usize, Option<usize>) {
    if total <= slots {
        return (total, None);
    }
    let shown = slots.saturating_sub(1);
    (shown, Some(total.saturating_sub(shown)))
}

/// How the queue panel spends `capacity` content rows.
///
/// Returns `(host rows, unacknowledged rows, sent-steer rows, overflow)`.
/// Host rows keep one row when the panel is taller than one line and anything
/// sits under them. `+N sending` counts only unacknowledged follow-ups and
/// steers that are not drawn, and that line is reserved before a host row is
/// chosen so a hidden host row is not painted and has no chip. Sent steers use
/// a leftover row; ones that do not fit are omitted and are not part of N.
pub(super) fn queue_panel_plan(
    capacity: usize,
    host_len: usize,
    unacked: usize,
    sent: usize,
) -> (usize, usize, usize, Option<usize>) {
    if capacity == 0 {
        return (0, 0, 0, None);
    }
    let under = unacked.saturating_add(sent);
    let yield_cap = if host_len > 0 {
        capacity.saturating_sub(1).min(3)
    } else {
        capacity
    };
    let mut under_slots = under.min(yield_cap);
    let mut host_slots = capacity.saturating_sub(under_slots).min(host_len);
    if unacked > under_slots && under_slots == 0 && host_slots > 0 {
        host_slots = host_slots.saturating_sub(1);
        under_slots = 1;
    }
    let (unacked_shown, overflow) = sending_overflow(unacked, under_slots);
    let used = unacked_shown.saturating_add(usize::from(overflow.is_some()));
    let sent_shown = sent.min(under_slots.saturating_sub(used));
    (host_slots, unacked_shown, sent_shown, overflow)
}

/// A draft the host acknowledged, still drawn until its row, queue entry or
/// steer acknowledgement takes over visibility.
pub(super) struct AcceptedDraft {
    action: UserAction,
    /// The queue item the host admitted it as, once `PromptAdmitted` named it.
    admitted: Option<String>,
    /// `PromptStarted` named that item: the next user row is its own.
    started: bool,
}

/// Captured steer text kept on the sent row. A few hundred characters is
/// enough to recognize the steer; the row clips to the pane on top of this.
const STEER_PREVIEW_CHARS: usize = 300;

/// One provisional row's text: the draft on one line, with its image count.
fn preview(draft: &Draft) -> String {
    let text = draft.text.trim().replace('\n', " ");
    match draft.images.len() {
        0 => text,
        1 => format!("{text} [1 image]"),
        n => format!("{text} [{n} images]"),
    }
}

fn clip_steer_preview(text: &str) -> String {
    text.chars().take(STEER_PREVIEW_CHARS).collect()
}

impl App {
    /// The host accepted the draft. Its row stays provisional
    /// until the host's own row, queue entry or steer acknowledgement takes
    /// over, so the draft does not disappear before its acknowledgement.
    pub(super) fn accept_pending_draft(&mut self, action: &UserAction) {
        if let Some(action) = self.remove_pending_draft(action) {
            self.accepted_drafts.push_back(AcceptedDraft {
                action,
                admitted: None,
                started: false,
            });
        }
    }

    /// The host acknowledged the oldest accepted steer. The returned text is
    /// the draft the queue panel shows; `None` means no accepted steer was
    /// waiting.
    pub(super) fn retire_accepted_steer(&mut self) -> Option<String> {
        let index = self
            .accepted_drafts
            .iter()
            .position(|accepted| matches!(accepted.action, UserAction::Steer(_)))?;
        let accepted = self.accepted_drafts.remove(index)?;
        let UserAction::Steer(draft) = accepted.action else {
            return None;
        };
        Some(clip_steer_preview(&preview(&draft)))
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
        let prompts: Vec<_> = self.provisional_prompts().collect();
        let (shown, overflow) = sending_overflow(prompts.len(), usize::from(area.height));
        let mut lines: Vec<Line<'static>> = prompts
            .into_iter()
            .take(shown)
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
        if let Some(hidden) = overflow {
            lines.push(
                Line::from(Span::styled(
                    format!("+{hidden} sending"),
                    self.theme.chrome_dim,
                ))
                .style(Style::default().bg(self.theme.user_bg)),
            );
        }
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
