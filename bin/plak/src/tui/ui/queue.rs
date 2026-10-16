//! Submit and queue actions.

use super::typing_guard::SEND_NOW_GAP;
use super::{
    App, QueueDrainState, QueueItem, ScrollBlock, UserAction, is_drop_command, is_quit_command,
    parse_slash, route_submit, slash,
};

impl App {
    /// Reconcile selection by stable identity, then replace the complete snapshot.
    pub(super) fn replace_queue_snapshot(&mut self, items: Vec<QueueItem>, drain: QueueDrainState) {
        let old_index = self.queue_selected;
        let selected_id = self.local_queue.get(old_index).map(|item| item.id.as_str());
        self.queue_selected = selected_id
            .and_then(|id| items.iter().position(|item| item.id == id))
            .unwrap_or_else(|| old_index.min(items.len().saturating_sub(1)));
        self.local_queue = items;
        if let Some(edit) = self.queue_editing.as_mut() {
            if let Some(item) = self.local_queue.iter().find(|item| item.id == edit.id) {
                edit.revision = item.revision;
            } else {
                // The item was sent or dropped: its text stays as an
                // ordinary draft, merged with the draft it displaced.
                let replaced = std::mem::take(&mut edit.replaced_draft);
                self.queue_editing = None;
                if !replaced.is_empty() {
                    self.restore_draft_text(replaced);
                }
            }
        }
        self.queue_len = self.local_queue.len();
        self.queue_drain = drain;
        if self.local_queue.is_empty() {
            self.queue_focused = false;
        }
        self.mark_dirty();
    }

    /// Submit the current input line.
    ///
    /// `steer` is true for Ctrl+Enter / Ctrl+S.
    pub(crate) fn submit_line(&mut self, steer: bool) -> Option<UserAction> {
        let line = self.input.trim().to_owned();
        // Empty Enter while busy + queue → send-now (plak: Steer top / FIFO head).
        // Attached images alone are a message.
        if line.is_empty() && (self.attachments.is_empty() || self.queue_focused) {
            if self.queue_focused {
                return self.queue_activate_selected(steer || self.busy);
            }
            // A plain `Enter` sends the oldest now only after SEND_NOW_GAP with
            // no typing key, `Enter` included: a doubled `Enter`, even a beat
            // apart, or a paste's trailing newline sends nothing.
            if self.busy
                && !self.local_queue.is_empty()
                && (steer || self.key_typing.since_typed >= SEND_NOW_GAP)
            {
                return self.queue_send_now_at(0); // FIFO head = oldest
            }
            return None;
        }
        if let Some(edit) = &self.queue_editing {
            let action = UserAction::EditQueued {
                id: edit.id.clone(),
                revision: edit.revision,
                text: line,
            };
            self.mark_dirty();
            return Some(action);
        }
        let parsed = parse_slash(&line);
        // Hold a prompt only while the agent is still preparing. Once the host
        // settled without one, it refuses the prompt with the reason and
        // returns the draft, rather than holding it here forever.
        if self.hosted
            && !self.agent_enabled
            && self.startup_pending
            && !is_quit_command(&line)
            && !is_drop_command(&line)
            && parsed.is_none()
        {
            self.last_prompt_at = Some(self.now());
            self.pending_prompt = Some(line);
            self.push_block(ScrollBlock::System(
                "queued until agent is ready".to_owned(),
            ));
            self.mark_dirty();
            return None;
        }
        self.clear_input();
        self.slash_open = false;
        self.queue_focused = false;
        self.history_browse = None;
        self.mark_dirty();

        // Over running background processes `/quit` asks first, as Ctrl+Q
        // does (spec 02 §5.7).
        if is_quit_command(&line) {
            return self.request_quit();
        }
        if is_drop_command(&line) {
            return self.queue_drop_last();
        }

        if let Some(parsed) = parsed {
            let effect = slash::resolve(&parsed, &self.slash_ctx());
            return self.apply_slash_effect(effect);
        }

        self.last_prompt_at = Some(self.now());
        let images = std::mem::take(&mut self.attachments);
        Some(route_submit(
            self.busy,
            steer,
            plakous_host_protocol::Draft::new(line, images),
        ))
    }

    /// Send-now the queue item at FIFO index (0 = oldest).
    pub(super) fn queue_send_now_at(&mut self, abs_idx: usize) -> Option<UserAction> {
        let item = self.local_queue.get(abs_idx)?;
        Some(UserAction::SendQueuedNow {
            id: item.id.clone(),
            revision: item.revision,
        })
    }

    pub(super) fn queue_activate_selected(&mut self, send_now: bool) -> Option<UserAction> {
        if self.local_queue.is_empty() {
            self.queue_focused = false;
            return None;
        }
        let idx = self.queue_selected.min(self.local_queue.len() - 1);
        if send_now {
            return self.queue_send_now_at(idx);
        }
        None
    }

    /// Enter on the focused queue sends wherever `Ctrl+S` does, including a
    /// restored-paused drain. Delivery unavailable cannot: the shell refuses.
    pub(super) fn queue_enter_sends(&self) -> bool {
        self.agent_enabled
    }

    /// Loads the selected queued prompt into the draft to edit it; the
    /// draft it replaces returns when the edit ends.
    pub(super) fn queue_edit_selected(&mut self) {
        let Some(item) = self.local_queue.get(self.queue_selected) else {
            return;
        };
        let text = item.text.clone();
        let id = item.id.clone();
        let revision = item.revision;
        let replaced_draft = match self.queue_editing.take() {
            // Switching edits keeps the original draft, not the first edit.
            Some(previous) => previous.replaced_draft,
            None => self.input.clone(),
        };
        self.set_input(text);
        self.queue_editing = Some(super::QueueEdit {
            id,
            revision,
            replaced_draft,
        });
        self.queue_focused = false;
        self.mark_dirty();
    }

    /// Ends a queue edit without saving it: the displaced draft returns as
    /// one undoable step.
    pub(super) fn cancel_queue_edit(&mut self) -> bool {
        let Some(edit) = self.queue_editing.take() else {
            return false;
        };
        self.set_input(edit.replaced_draft);
        self.mark_dirty();
        true
    }

    pub(super) fn queue_drop_selected(&mut self) -> Option<UserAction> {
        let idx = self
            .queue_selected
            .min(self.local_queue.len().checked_sub(1)?);
        self.queue_delete_at(idx)
    }

    pub(super) fn queue_drop_last(&self) -> Option<UserAction> {
        self.queue_delete_at(self.local_queue.len().checked_sub(1)?)
    }

    pub(super) fn queue_delete_at(&self, abs_idx: usize) -> Option<UserAction> {
        let item = self.local_queue.get(abs_idx)?;
        Some(UserAction::DeleteQueued {
            id: item.id.clone(),
            revision: item.revision,
        })
    }
}
