//! Host event application.

use super::{
    App, ControlTab, DisplayMode, Duration, HostEvent, PROMPT_HISTORY_LIMIT, ResumeMenu,
    ScrollBlock, SettingsMenu, UserAction, blocks, terminal_state_keeping_duration,
    tool_is_running, unix_now, unknown_context_usage,
};
use crate::tui::SurfaceObservation;
use plakous_host_protocol::status::HostPhase;

impl App {
    /// Retire observations and handles owned by the source session, not terminal preferences.
    fn reset_session_state(&mut self) {
        self.context_usage = unknown_context_usage();
        self.last_turn_usage = None;
        self.last_turn = None;
        self.last_prompt_at = None;
        self.idle_clock_painted = None;
        self.pending_retry = None;
        self.turn_started = None;
        self.phase_started = None;
        self.model_request_started = None;
        self.stream_discard_from = 0;
        self.request_slot_wait = None;
        self.credential_wait = None;
        self.cancel_pending = false;
        self.prompt_row_awaiting_busy = false;
        self.busy = false;
        self.phase = HostPhase::Idle;
        self.status_detail = None;
        self.clear_pending = false;
        self.quit_arm = None;
        self.esc_clear_arm = None;
        self.open_approval = None;
        self.pending_continuation = plakous_host_protocol::PendingContinuation::default();
        self.local_queue.clear();
        self.queue_len = 0;
        self.queue_drain = crate::tui::QueueDrainState::LiveAutoDrain;
        self.queue_focused = false;
        self.queue_selected = 0;
        self.cancel_queue_edit();
        self.pending_queue_edit = None;
        self.pending_drafts.clear();
        self.accepted_drafts.clear();
        self.prompt_history.clear();
        self.recovered_drafts.clear();
        self.history_browse = None;
        self.prompt_history_hits.clear();
        self.queued_steers_retired = 0;
        self.composer_chrome = super::composer_chrome::ComposerChrome::default();
        self.agent_tree = None;
        self.tree_navigator = None;
        self.agents_unavailable = None;
        self.tree_epoch_state = None;
        self.tree_live_descendants = 0;
        self.pending_open_tree = self.agent_enabled;
        self.agent_acks = super::tree::AgentAcknowledgements::default();
        self.call_details = super::call_detail::CallDetails::default();
        self.session_info = crate::tui::presentation::SessionInfoView::default();
        self.session_rows.clear();
        self.session_row_hits.clear();
        self.tab_copy_text.clear();
        self.info_overlay = None;
        self.capacity_recovery = None;
        self.capacity_recovery_action = None;
        if let Some(reader) = &mut self.usage_decoration {
            reader.invalidate();
        }
        self.mark_dirty();
    }

    /// Marks or removes the assistant text and reasoning the failed request
    /// streamed. Only blocks from the current model request are touched, so an
    /// earlier request of the same turn stays. A tool row of that response
    /// does not hide the text that preceded it. A committed row ends the walk.
    fn discard_streamed_assistant(&mut self, dim: bool) {
        let mut drop_at = Vec::new();
        let mut dim_at = Vec::new();
        let floor = self.stream_discard_from.min(self.pane.blocks.len());
        for index in (floor..self.pane.blocks.len()).rev() {
            if self
                .pane
                .keyed
                .key_at(index)
                .is_some_and(|key| key.anchor < 1_u64 << 63)
            {
                break;
            }
            match &self.pane.blocks[index] {
                ScrollBlock::Assistant(_) | ScrollBlock::Thinking { .. } if dim => {
                    dim_at.push(index);
                }
                ScrollBlock::Assistant(_) | ScrollBlock::Thinking { .. } => drop_at.push(index),
                ScrollBlock::Discarded { .. } | ScrollBlock::Tool { .. } => {}
                _ => break,
            }
        }
        for index in dim_at {
            let (text, thinking) = match &mut self.pane.blocks[index] {
                ScrollBlock::Assistant(text) => (std::mem::take(text), false),
                ScrollBlock::Thinking { text, .. } => (std::mem::take(text), true),
                _ => continue,
            };
            self.pane.blocks[index] = ScrollBlock::Discarded { text, thinking };
            self.mark_block_changed(index);
        }
        for index in drop_at {
            self.remove_keyed_block(index);
        }
        self.pane.assistant_stream_active = false;
    }

    /// Apply a host event (status / transcript / structured blocks).
    pub(crate) fn apply_host_event(&mut self, event: HostEvent) {
        self.apply_host_event_state(event);
        // A stop pressed for a sent prompt the host then refused without a
        // busy edge has nothing left to settle.
        if self.cancel_pending && !self.root_stoppable() {
            self.cancel_pending = false;
        }
        // An open pop-up whose target stopped being active closes.
        self.retire_stale_confirm();
    }

    fn apply_host_event_state(&mut self, event: HostEvent) {
        if let HostEvent::TranscriptStream { scope, delta } = event {
            use plakous_host_protocol::views::transcript::{
                TranscriptScope, TranscriptStreamDelta,
            };
            let TranscriptScope::Agent(id) = scope else {
                return;
            };
            if self.viewed_agent_id() != Some(id.as_str()) {
                return;
            }
            let event = match delta {
                TranscriptStreamDelta::Assistant(text) => HostEvent::AssistantDelta(text),
                TranscriptStreamDelta::AssistantDone => HostEvent::AssistantDone,
                TranscriptStreamDelta::Reasoning { location_key, text } => {
                    HostEvent::ReasoningDelta { location_key, text }
                }
                TranscriptStreamDelta::Tool(tool) => HostEvent::Row(
                    plakous_host_protocol::views::transcript::TranscriptRow::Tool(*tool),
                ),
            };
            self.semantic_revision = self.semantic_revision.saturating_add(1);
            self.apply_live_op(event);
            return;
        }
        if self.ends_agent_view(&event) {
            self.close_agent_view();
            // Its target belonged to the session that was attached.
            if self.destructive_confirm.take().is_some() {
                self.mark_dirty();
            }
        }
        let scope = match &event {
            HostEvent::PresentationSnapshot(snapshot) => Some(&snapshot.scope),
            HostEvent::TranscriptPatch(patch) => Some(&patch.scope),
            _ => None,
        };
        if let Some(plakous_host_protocol::views::transcript::TranscriptScope::Agent(id)) = scope {
            if self.viewed_agent_id() == Some(id.as_str()) {
                self.semantic_revision = self.semantic_revision.saturating_add(1);
                self.apply_presentation(event);
            }
            return;
        }
        if let HostEvent::CallDetail(detail) = &event
            && detail.agent_id.is_some()
        {
            self.semantic_revision = self.semantic_revision.saturating_add(1);
            if let HostEvent::CallDetail(detail) = event {
                self.apply_call_detail(detail);
            }
            return;
        }
        self.with_root_pane(|app| app.apply_root_host_event(event));
    }

    fn install_keyed_transcript(
        &mut self,
        rows: Vec<plakous_host_protocol::views::transcript::TranscriptItem>,
        resume_stream: bool,
        keep_local: bool,
        namespace: Option<String>,
    ) {
        #[cfg(test)]
        {
            self.pane.keyed_rebuilds += 1;
        }
        let root = self.viewed_agent_id().is_none();
        if root {
            self.accepted_drafts.clear();
        }
        let streaming = resume_stream || self.pane.assistant_stream_active;
        self.pane.assistant_stream_active = false;
        let same_session = self.pane.presentation_session == namespace;
        let mut restore = self.take_keyed_restore(keep_local && same_session, same_session, root);
        self.pane.presentation_session = namespace;
        if !same_session {
            self.pane.tool_started.clear();
            self.pane.thinking_started.clear();
        }
        self.restore_tip_clocks(rows.iter().map(|item| &item.row));
        let full_window = rows.len() == crate::tui::presentation::MAX_SNAPSHOT_ROWS;
        let live_assistant = rows.last().is_some_and(|item| {
            item.key.anchor >= 1_u64 << 63
                && matches!(
                    &item.row,
                    crate::store::sessions::TranscriptRow::Assistant { .. }
                )
        });
        let mut blocks = self.keyed_snapshot_blocks(rows);
        let live_tools: std::collections::BTreeSet<_> = blocks
            .iter()
            .filter_map(|block| match block {
                ScrollBlock::Tool {
                    call_id: Some(id),
                    state,
                    ..
                } if tool_is_running(state) => Some(id.clone()),
                _ => None,
            })
            .collect();
        let live_thinking: std::collections::BTreeSet<_> = blocks
            .iter()
            .filter_map(|block| match block {
                ScrollBlock::Thinking {
                    location_key,
                    sealed: false,
                    ..
                } => Some(location_key.clone()),
                _ => None,
            })
            .collect();
        self.pane
            .tool_started
            .retain(|id, _| live_tools.contains(id));
        self.pane
            .thinking_started
            .retain(|id, _| live_thinking.contains(id));
        let now = self.now();
        for id in live_tools {
            self.pane.tool_started.entry(id).or_insert(now);
        }
        for id in live_thinking {
            self.pane.thinking_started.entry(id).or_insert(now);
        }
        if root {
            self.restore_opening_note(&mut blocks, full_window);
        }
        self.restore_local_keyed_blocks(&mut blocks, &mut restore);
        self.restore_refusals(&mut blocks);
        Self::restore_open_notes(&mut blocks, &mut restore);
        self.pane.blocks = blocks;
        let running = self.finish_keyed_restore(restore);
        self.refresh_keyed_item_details(running);
        self.pane.assistant_stream_active = streaming && live_assistant;
        self.mark_transcript_replaced();
        self.pane
            .transcript_cache
            .require_blocks(self.pane.selection.block_span());
        self.warm_transcript_projection();
        self.pane
            .transcript_cache
            .materialize_required(&self.pane.blocks);
        self.pane
            .selection
            .resolve_blocks(self.pane.transcript_cache.layout());
    }

    /// Keeps this session's refusal notes past a snapshot that replaced them,
    /// each after the keyed row it followed, so it scrolls away with history.
    fn restore_refusals(&mut self, blocks: &mut Vec<ScrollBlock>) {
        let session = &self.pane.presentation_session;
        self.pane
            .refusals
            .retain(|refusal| refusal.session == *session);
        for refusal in &self.pane.refusals {
            if blocks
                .iter()
                .any(|block| matches!(block, ScrollBlock::Error(text) if *text == refusal.note))
            {
                continue;
            }
            let at = self.pane.keyed.after(refusal.anchor, blocks.len());
            self.pane.keyed.splice(at, 0, 1);
            blocks.insert(at, ScrollBlock::Error(refusal.note.clone()));
        }
    }

    fn refresh_keyed_item_details(&mut self, running: std::collections::BTreeSet<String>) {
        let ended: Vec<_> = self
            .pane
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| match block {
                ScrollBlock::Tool {
                    call_id: Some(id),
                    state,
                    ..
                } if !tool_is_running(state) && running.contains(id.as_str()) => {
                    Some((index, id.clone()))
                }
                _ => None,
            })
            .collect();
        for (index, id) in ended {
            if let Some(key) = self.detail_key(&id) {
                self.call_details.note_ended(key);
            }
            self.request_item_detail(index);
        }
    }

    fn apply_presentation(&mut self, event: HostEvent) {
        match event {
            HostEvent::PresentationSnapshot(snapshot) => {
                // Rebind publishes the successor snapshot before Binding.
                // Its header scopes UI identity; it does not change the binding.
                let namespace = snapshot
                    .items
                    .first()
                    .and_then(|item| match &item.row {
                        crate::store::sessions::TranscriptRow::SessionHeader {
                            session_id, ..
                        } if snapshot.scope
                            == plakous_host_protocol::views::transcript::TranscriptScope::Root =>
                        {
                            Some(session_id.clone())
                        }
                        _ => None,
                    })
                    .or_else(|| self.session_id.clone());
                if self.busy
                    && self.pane.presentation_ready
                    && self.pane.presentation_session == namespace
                    && snapshot.revision <= self.pane.presentation.revision()
                {
                    return;
                }
                let keep_local = self.busy
                    && self.pane.presentation_ready
                    && self.pane.presentation.scope() == &snapshot.scope;
                let scope = snapshot.scope.clone();
                if let Err(error) = self.pane.presentation.replace(&snapshot) {
                    let note = format!(
                        "invalid presentation snapshot refused ({error:?}); kept the last good view and requested a resync"
                    );
                    if !self
                        .pane
                        .refusals
                        .iter()
                        .any(|refusal| refusal.session == namespace && refusal.note == note)
                    {
                        tracing::warn!(
                            target: "plak.presentation",
                            ?scope,
                            ?error,
                            rows = snapshot.items.len(),
                            "invalid presentation snapshot refused"
                        );
                        let anchor = self.pane.keyed.committed_anchor(self.pane.blocks.len());
                        self.pane.blocks.push(ScrollBlock::Error(note.clone()));
                        self.pane.refusals.push(super::pane::Refusal {
                            session: namespace,
                            note,
                            anchor,
                        });
                    }
                    self.pane.resync.get_or_insert(scope);
                    self.mark_transcript_replaced();
                    return;
                }
                if matches!(
                    scope,
                    plakous_host_protocol::views::transcript::TranscriptScope::Agent(_)
                ) {
                    // The host served the agent transcript: the open landed.
                    self.reset_rearm(super::unsent::RearmReset::AgentTranscript);
                }
                self.presentation_replaced();
                self.install_keyed_transcript(snapshot.items, true, keep_local, namespace);
            }
            HostEvent::TranscriptPatch(patch) => {
                if self.pane.resync.is_some() {
                    self.rearm_transcript_resync(patch.from_revision);
                    return;
                }
                let scope = patch.scope.clone();
                let from_revision = patch.from_revision;
                if !self.pane.presentation_ready
                    || self.pane.presentation_session != self.session_id
                    || self.pane.presentation.apply(&patch).is_err()
                {
                    self.pane.resync = Some(scope);
                    self.rearm_transcript_resync(from_revision);
                    return;
                }
                self.patch_keyed_blocks(patch.upserts, patch.deletes);
            }
            _ => {}
        }
    }

    /// One live transcript op, of the root or of the open agent view, applied
    /// to the pane on screen. The root's turn bookkeeping (the HUD's model
    /// and phase clocks) runs only for the root pane.
    #[allow(
        clippy::too_many_lines,
        reason = "the live transcript arms stay one match, as the host sends them"
    )]
    fn apply_live_op(&mut self, event: HostEvent) {
        let root = self.viewed_agent_id().is_none();
        match event {
            HostEvent::Row(row) => {
                let local = matches!(
                    &row,
                    plakous_host_protocol::views::transcript::TranscriptRow::Notice(notice)
                        if notice.scope == plakous_host_protocol::views::transcript::NoticeScope::Settings
                );
                if let Some(mut block) = ScrollBlock::from_row(row) {
                    // Projector settlements replace previews (or restored rows)
                    // by call identity, without moving them past the answer.
                    if let ScrollBlock::Tool {
                        call_id: Some(call_id),
                        mode,
                        manual,
                        group_open,
                        view,
                        ..
                    } = &mut block
                        && let Some(index) = self.pane.blocks.iter().rposition(|existing| {
                            matches!(existing, ScrollBlock::Tool { call_id: Some(id), .. } if id == call_id)
                        })
                    {
                        if let ScrollBlock::Tool { mode: old_mode, manual: old_manual, group_open: old_group_open, view: old_view, .. } = &self.pane.blocks[index] {
                            *mode = *old_mode;
                            *manual = *old_manual;
                            *group_open = *old_group_open;
                            view.clone_from(old_view);
                        }
                        self.pane.transcript_bytes = self.pane.transcript_bytes
                            .saturating_sub(self.pane.blocks[index].retained_bytes())
                            .saturating_add(block.retained_bytes());
                        self.pane.blocks[index] = block;
                        self.note_live_block(index);
                        self.mark_block_changed(index);
                        return;
                    }
                    if local {
                        self.push_block(block);
                    } else {
                        self.push_live_block(block);
                    }
                }
            }
            HostEvent::AssistantDelta(delta) => {
                if root {
                    self.model_request_started = None;
                }
                let starts_block = !self.pane.assistant_stream_active
                    || !matches!(self.pane.blocks.last(), Some(ScrollBlock::Assistant(_)));
                self.pane.transcript_bytes = self.pane.transcript_bytes.saturating_add(delta.len());
                if !self.pane.assistant_stream_active {
                    // Responding is a new phase (§5.4 segment 3).
                    if root {
                        self.phase_started = Some(self.now());
                    }
                    self.pane.blocks.push(ScrollBlock::Assistant(delta));
                    self.note_live_block(self.pane.blocks.len() - 1);
                    self.pane.assistant_stream_active = true;
                } else if let Some(ScrollBlock::Assistant(text)) = self.pane.blocks.last_mut() {
                    text.push_str(&delta);
                } else {
                    self.pane.blocks.push(ScrollBlock::Assistant(delta));
                    self.note_live_block(self.pane.blocks.len() - 1);
                }
                self.enforce_transcript_retention();
                if starts_block {
                    self.mark_transcript_structural();
                } else {
                    self.mark_assistant_tail_changed();
                }
            }
            HostEvent::AssistantDone => {
                // Keep the last streamed assistant block; clear the stream cursor.
                self.pane.assistant_stream_active = false;
                self.mark_dirty();
            }
            HostEvent::ToolLine {
                call_id,
                name,
                state,
                subject,
                detail,
                parent_call_id,
                cell_id,
                kind,
                duration_ms,
                started_at_ms,
                finished_at_ms,
                wait,
            } => {
                let timing_key = call_id.clone().unwrap_or_else(|| name.clone());
                if root
                    && (tool_is_running(&state)
                        || kind == crate::tui::presentation::ActivityKind::Compact)
                {
                    self.model_request_started = None;
                }
                // Prefer call_id match; else last running row of this name.
                let idx = self.pane.blocks.iter().rposition(|b| match b {
                    ScrollBlock::Tool {
                        call_id: cid,
                        name: n,
                        state: s,
                        ..
                    } => {
                        if let (Some(a), Some(b)) = (call_id.as_ref(), cid.as_ref()) {
                            a == b
                        } else {
                            n == &name && tool_is_running(s)
                        }
                    }
                    _ => false,
                });
                let existing_is_terminal = idx.is_some_and(|idx| match &self.pane.blocks[idx] {
                    ScrollBlock::Tool { state, .. } => !tool_is_running(state),
                    _ => false,
                });
                let mut elapsed = None;
                if tool_is_running(&state) {
                    // A Compose head the host put back in flight after a yield is
                    // not a new measurement: re-arming the clock replaced the
                    // cell's own `done:12.3s` with the last segment's `0.3s`.
                    // Only a row that is not already terminal
                    // arms one.
                    if !existing_is_terminal {
                        // §5.4 segment 3 is the *phase* timer: a new running
                        // tool is a new phase. Left at the turn start it printed
                        // the turn elapsed twice.
                        if root && !self.pane.tool_started.contains_key(&timing_key) {
                            self.phase_started = Some(self.now());
                        }
                        let now = self.now();
                        self.pane
                            .tool_started
                            .entry(timing_key.clone())
                            .or_insert(now);
                    }
                } else {
                    // The host's own `started_at_ms` is the authority, and it is
                    // the same stamp a resumed transcript derives its duration
                    // from, so live and resumed rows agree by construction.
                    // The renderer's clock is the fallback for
                    // a call the host holds no start for.
                    // Keyed by agent and call: the pane on screen decides.
                    if let Some(key) = call_id.as_deref().and_then(|id| self.detail_key(id)) {
                        self.call_details.note_ended(key);
                    }
                    let measured = self
                        .pane
                        .tool_started
                        .remove(&timing_key)
                        .map(|started| self.now().saturating_duration_since(started));
                    elapsed = started_at_ms
                        .zip(finished_at_ms)
                        .and_then(|(start, finish)| finish.checked_sub(start))
                        .or(duration_ms)
                        .map(Duration::from_millis)
                        .or(measured);
                }
                let state = blocks::presented_tool_state(&state, elapsed);
                let before_bytes = idx.map_or(0, |idx| self.pane.blocks[idx].retained_bytes());
                let lineage_bearing = parent_call_id.is_some()
                    || cell_id.is_some()
                    || kind == crate::tui::presentation::ActivityKind::Compose;
                if let Some(idx) = idx
                    && let ScrollBlock::Tool {
                        call_id: existing_id,
                        parent_call_id: existing_parent,
                        cell_id: existing_cell,
                        kind: existing_kind,
                        state: existing_state,
                        subject: existing_subject,
                        detail: existing_detail,
                        wait: existing_wait,
                        started_at_ms: existing_started,
                        finished_at_ms: existing_finished,
                        ..
                    } = &mut self.pane.blocks[idx]
                {
                    if wait.is_some() {
                        *existing_wait = wait;
                    }
                    if started_at_ms.is_some() {
                        *existing_started = started_at_ms;
                    }
                    if existing_id.is_none() {
                        *existing_id = call_id;
                    }
                    if existing_parent.is_none() {
                        existing_parent.clone_from(&parent_call_id);
                    }
                    if existing_cell.is_none() {
                        existing_cell.clone_from(&cell_id);
                    }
                    *existing_kind = kind;
                    if tool_is_running(&state) {
                        // §5.8: a cell that yielded and then dispatched again is
                        // in flight, and the host says so. A Compose head is the
                        // one row a `running` after a terminal may move back —
                        // otherwise `✓ Compose` stayed painted over running (and,
                        // after a cancel, over `⊘`) children.
                        // Every other row keeps its terminal: the host never
                        // un-terminalises an ordinary tool.
                        if kind == crate::tui::presentation::ActivityKind::Compose
                            && !tool_is_running(existing_state)
                        {
                            "running".clone_into(existing_state);
                            *existing_finished = None;
                            if subject.is_some() {
                                existing_subject.clone_from(&subject);
                            }
                        } else if let Some(s) = subject.clone()
                            && existing_subject.as_ref().is_none_or(String::is_empty)
                        {
                            *existing_subject = Some(s);
                        }
                        // Only fill detail when empty (progress may add a hint).
                        if existing_detail.as_ref().is_none_or(String::is_empty) && detail.is_some()
                        {
                            *existing_detail = detail;
                        }
                    } else {
                        // Every terminal keeps a duration the row already
                        // recorded, `cancelled` and `error` included: they are
                        // terminal states like any other and dropping the
                        // elapsed there lost it wholesale.
                        *existing_state = terminal_state_keeping_duration(existing_state, state);
                        if finished_at_ms.is_some() {
                            *existing_finished = finished_at_ms;
                        }
                        if subject.is_some() {
                            *existing_subject = subject;
                        }
                        if detail.is_some() {
                            *existing_detail = detail;
                        }
                    }
                    self.pane.transcript_bytes = self
                        .pane
                        .transcript_bytes
                        .saturating_sub(before_bytes)
                        .saturating_add(self.pane.blocks[idx].retained_bytes());
                    self.enforce_transcript_retention();
                    // An item opened while it ran asks for its canonical
                    // detail once it ends.
                    self.request_item_detail(idx);
                    // Lineage and grouping make a row's shape depend on its
                    // neighbours, so a lineage-bearing or groupable row is a
                    // structural change rather than a one-block splice.
                    let groupable = self.render_prefs.group_tool_calls
                        && (idx + 1 < self.pane.blocks.len()
                            || matches!(
                                blocks::tool_verb(&name, kind),
                                blocks::ToolVerb::Read
                                    | blocks::ToolVerb::Search
                                    | blocks::ToolVerb::Agent
                                    | blocks::ToolVerb::SpawnAgent
                                    | blocks::ToolVerb::WaitAgent
                            ));
                    if lineage_bearing || groupable {
                        self.mark_transcript_structural();
                    } else {
                        self.mark_block_changed(idx);
                    }
                    return;
                }
                let optional_id = call_id.clone();
                let mut block = ScrollBlock::from_tool_item(
                    plakous_host_protocol::views::transcript::ToolItem {
                        call_id: call_id.unwrap_or_default(),
                        parent_call_id,
                        cell_id,
                        kind,
                        name,
                        exposed_name: None,
                        state,
                        subject,
                        detail,
                        wait,
                        started_at_ms,
                        finished_at_ms,
                    },
                );
                if let ScrollBlock::Tool { call_id, .. } = &mut block {
                    *call_id = optional_id;
                }
                self.push_live_block(block);
            }
            HostEvent::ReasoningDelta { location_key, text } => {
                if root {
                    self.model_request_started = None;
                }
                let now = self.now();
                self.pane
                    .thinking_started
                    .entry(location_key.clone())
                    .or_insert(now);
                let existing = self.pane.blocks.iter().rposition(|block| {
                    matches!(block, ScrollBlock::Thinking { location_key: key, sealed: false, .. } if *key == location_key)
                });
                if let Some(idx) = existing {
                    let before = self.pane.blocks[idx].retained_bytes();
                    if let ScrollBlock::Thinking { text: body, .. } = &mut self.pane.blocks[idx] {
                        body.push_str(&text);
                    }
                    self.pane.transcript_bytes = self
                        .pane
                        .transcript_bytes
                        .saturating_sub(before)
                        .saturating_add(self.pane.blocks[idx].retained_bytes());
                    self.enforce_transcript_retention();
                    self.mark_block_changed(idx);
                } else {
                    self.push_live_block(ScrollBlock::Thinking {
                        location_key,
                        text,
                        sealed: false,
                        elapsed_ms: None,
                        // §5.4: a streaming thought is one spinner row.
                        mode: DisplayMode::Collapsed,
                        manual: false,
                    });
                }
            }
            HostEvent::ReasoningSealed { location_key } => {
                let elapsed = self
                    .pane
                    .thinking_started
                    .remove(&location_key)
                    .map(|start| {
                        u64::try_from(self.now().saturating_duration_since(start).as_millis())
                            .unwrap_or(u64::MAX)
                    });
                let Some(idx) = self.pane.blocks.iter().rposition(|block| {
                    matches!(block, ScrollBlock::Thinking { location_key: key, sealed: false, .. } if *key == location_key)
                }) else {
                    return;
                };
                if let ScrollBlock::Thinking {
                    sealed,
                    elapsed_ms,
                    mode,
                    manual,
                    ..
                } = &mut self.pane.blocks[idx]
                {
                    *sealed = true;
                    *elapsed_ms = elapsed;
                    if !*manual {
                        *mode = DisplayMode::Collapsed;
                    }
                }
                self.mark_transcript_structural();
            }
            _ => {}
        }
    }

    /// A session switch (clear, open, resume, fork) ends any agent view: its
    /// agent belongs to the tree of the session that was attached. A
    /// same-session presentation snapshot (a refusal, a saturation
    /// resnapshot, a rebind) is not one.
    fn ends_agent_view(&self, event: &HostEvent) -> bool {
        match event {
            HostEvent::ClearView | HostEvent::SessionOpening => true,
            HostEvent::Binding {
                session_id: Some(session),
                ..
            } => self.session_id.as_deref() != Some(session.as_str()),
            _ => false,
        }
    }

    /// One host event of the root session. Runs with the root pane on
    /// screen, whatever the main view shows.
    #[allow(
        clippy::too_many_lines,
        reason = "single host event match keeps protocol handling colocated"
    )]
    fn apply_root_host_event(&mut self, event: HostEvent) {
        self.semantic_revision = self.semantic_revision.saturating_add(1);
        if matches!(
            event,
            HostEvent::AssistantDelta(_)
                | HostEvent::ReasoningDelta { .. }
                | HostEvent::ToolLine { .. }
                | HostEvent::SetBusy(false)
        ) {
            self.pending_retry = None;
        }
        match event {
            HostEvent::TranscriptStream { .. } => {
                unreachable!("scoped stream routed before root events")
            }
            HostEvent::StorageAdmissionRefused(diagnostic) => self.apply_live_op(HostEvent::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::warning(
                    plakous_host_protocol::views::transcript::NoticeScope::Settings,
                    crate::notice_text::storage_admission(diagnostic.as_str()),
                ),
            )),
            HostEvent::RestoredPause(pause) => self.apply_live_op(HostEvent::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::warning(
                    plakous_host_protocol::views::transcript::NoticeScope::Queue,
                    crate::notice_text::restored_pause(
                        pause,
                        crate::notice_text::RemedyContext {
                            frontend: crate::notice_text::Frontend::Tui,
                            session_id: None,
                        },
                    ),
                ),
            )),
            HostEvent::ModelRetry(retry) => {
                if retry.output_discarded {
                    self.discard_streamed_assistant(true);
                }
                self.pending_retry = Some((retry, self.now()));
                self.mark_dirty();
            }
            HostEvent::PromptAdmitted(item) => {
                self.note_admitted_draft(item.id);
                self.mark_dirty();
            }
            HostEvent::PromptStarted(Some(item)) => self.note_started_draft(&item.id),
            HostEvent::PromptCorrelationReset => {
                self.retire_unnamed_drafts();
                self.mark_dirty();
            }
            HostEvent::ToolInvocationOrigin { .. }
            | HostEvent::PromptStarted(None)
            | HostEvent::IntentStarted(_) => {}
            event @ (HostEvent::PresentationSnapshot(_) | HostEvent::TranscriptPatch(_)) => {
                self.apply_presentation(event);
            }
            HostEvent::ProviderSnapshot { providers } => {
                self.providers = providers;
                self.mark_dirty();
            }
            HostEvent::Activity { label, active } => {
                self.status_detail = active.then_some(label);
                self.startup_pending &= active;
                self.mark_dirty();
            }
            HostEvent::AgentAttached(attached) => {
                self.agent_enabled = attached;
                self.startup_pending = false;
                if attached {
                    crate::tui_timing::host_ready();
                    self.pending_open_tree = true;
                }
                self.mark_dirty();
            }
            event @ (HostEvent::Row(_)
            | HostEvent::AssistantDelta(_)
            | HostEvent::AssistantDone
            | HostEvent::ToolLine { .. }
            | HostEvent::ReasoningDelta { .. }
            | HostEvent::ReasoningSealed { .. }) => {
                if matches!(
                    event,
                    HostEvent::Row(
                        plakous_host_protocol::views::transcript::TranscriptRow::Steer { .. }
                    )
                ) {
                    self.retire_accepted_steer();
                }
                self.apply_live_op(event);
            }
            HostEvent::UserMessage { text, sent_at_ms } => {
                self.retire_started_draft();
                self.push_live_block(ScrollBlock::User { text, sent_at_ms });
            }
            HostEvent::OutputDiscarded { dim } => {
                self.discard_streamed_assistant(dim);
                self.mark_dirty();
            }
            HostEvent::ModelRefusal { detail } => {
                self.discard_streamed_assistant(false);
                let message = detail.map_or_else(
                    || "model refused the request".into(),
                    |detail| format!("model refused the request: {detail}"),
                );
                self.push_block(ScrollBlock::System(message));
            }
            HostEvent::Binding {
                session_id,
                account,
                model,
                effort,
                extra,
                composition,
                protocol,
                effective_presentation,
            } => {
                if let Some(s) = session_id {
                    // A rebind to another session retires the previous turn's
                    // lifecycle projection: its correlation is session-scoped.
                    if self.session_id.as_deref() != Some(s.as_str()) {
                        self.reset_session_state();
                        self.drop_reload_requests_of_other_sessions(&s);
                    }
                    self.session_id = Some(s);
                }
                // A rebind to another vertical retires the reading
                // the previous one reported; the host sends the new binding's
                // own on its next observation.
                if self.account != account || self.model != model {
                    self.context_usage = unknown_context_usage();
                }
                self.composer_chrome.profile = None;
                self.composer_chrome.refresh_pending = true;
                self.account = account;
                self.model = model;
                self.effort = effort;
                self.model_extra = extra.filter(|value| !value.is_empty());
                self.composition = composition;
                self.protocol = protocol;
                self.effective_presentation = effective_presentation;
                self.mark_dirty();
            }
            HostEvent::ClearView => {
                self.accepted_drafts.clear();
                self.queued_steers_retired = 0;
                self.clear_pending = false;
                self.last_turn = None;
                self.pane.blocks.clear();
                self.stream_discard_from = 0;
                self.clear_presentation();
                self.pane.assistant_stream_active = false;
                self.open_approval = None;
                self.pane.scroll_from_bottom = 0;
                // The cleared session's processes are not this one's; its
                // own list follows when it has one.
                self.apply_process_snapshot(std::sync::Arc::new(Vec::new()));
                self.mark_transcript_replaced();
            }
            HostEvent::Usage(usage) => {
                self.context_usage = usage;
                self.mark_dirty();
            }
            HostEvent::TurnUsage {
                input_tokens,
                output_tokens,
                reasoning_tokens,
                cached_input_tokens,
                cache_write_tokens,
                model_operations,
                reporting_model_operations,
                // Chrome reads provider-reported totals; the prepared-input
                // peak is for clients measuring context growth.
                peak_prepared_input_tokens: _,
                terminal: _,
            } => {
                self.last_turn_usage = Some((
                    input_tokens,
                    output_tokens,
                    reasoning_tokens,
                    cached_input_tokens,
                    cache_write_tokens,
                    model_operations,
                    reporting_model_operations,
                ));
                // Usage is inspector/chrome state, never conversation content.
                self.mark_dirty();
            }
            HostEvent::TurnLifecycle(lifecycle) => {
                if lifecycle.phase != plakous_host_protocol::TurnPhase::Admitted {
                    self.composer_chrome.refresh_pending = true;
                }
                self.last_turn = Some(lifecycle);
                self.mark_dirty();
            }
            HostEvent::SubscriptionUsage(usage) => {
                self.subscription_usage = usage;
                self.mark_dirty();
            }
            HostEvent::UsageLimits(limits) => {
                self.refresh_failure_notes.clear();
                if let Some(reader) = &mut self.usage_decoration {
                    reader.request(limits.clone());
                }
                self.usage_limits = limits;
                self.mark_dirty();
            }
            HostEvent::SessionInfo(info) => {
                self.session_info = info;
                self.mark_dirty();
            }
            HostEvent::Extension(SurfaceObservation::UsageRefreshAccounts(accounts)) => {
                self.reset_rearm(super::unsent::RearmReset::RereadUsage);
                self.usage_refresh.accounts = accounts;
                self.usage_refresh.focus = self
                    .usage_refresh
                    .focus
                    .min(self.usage_refresh.accounts.len().saturating_sub(1));
                self.mark_dirty();
            }
            HostEvent::Extension(SurfaceObservation::UsageCredentialRefreshed {
                account,
                error,
            }) => {
                if self.usage_refresh.active.as_ref() == Some(&account) {
                    self.usage_refresh.active = None;
                    self.usage_refresh.error = error.map(|reason| (account, reason));
                    self.usage_refresh.pending_read = true;
                    self.mark_dirty();
                }
            }
            HostEvent::Extension(SurfaceObservation::Profiles(view)) => {
                self.apply_profiles_view(view);
            }
            HostEvent::Extension(SurfaceObservation::SessionProfile(profile)) => {
                self.composer_chrome.profile = Some(profile);
                self.mark_dirty();
            }
            HostEvent::Extension(SurfaceObservation::WorkspaceChrome(label)) => {
                self.workspace_chrome = label;
                self.reset_rearm(super::unsent::RearmReset::ComposerChrome);
                self.mark_dirty();
            }
            HostEvent::Extension(SurfaceObservation::McpOverview(body)) => {
                self.info_overlay = Some((
                    "mcp".to_owned(),
                    crate::tui::surface_views::mcp_overview(&body),
                ));
                self.info_overlay_scroll = super::modal_chrome::ScrollState::default();
                self.mark_dirty();
            }
            HostEvent::Extension(SurfaceObservation::ProfileApplied { name, outcome }) => {
                self.composer_chrome.refresh_pending = true;
                self.apply_profile_applied(&name, outcome);
            }
            HostEvent::Extension(SurfaceObservation::AgentAcknowledgements {
                tree_id,
                attempts,
            }) => self.apply_agent_acknowledgements(tree_id, attempts),
            HostEvent::Extension(SurfaceObservation::AgentAcknowledgementsUnavailable {
                tree_id,
            }) => self.agent_acknowledgements_unavailable(&tree_id),
            HostEvent::Extension(SurfaceObservation::ProfileRefused { name, reason }) => {
                self.apply_profile_refused(&name, &reason);
            }
            HostEvent::Extension(SurfaceObservation::BuildStatus { newer_installed }) => {
                self.apply_build_status(newer_installed);
            }
            HostEvent::Extension(SurfaceObservation::ReloadRequested(request)) => {
                self.apply_reload_request(request);
            }
            HostEvent::Extension(SurfaceObservation::ReloadProbed { target, refusal }) => {
                self.apply_reload_probed(Some((target.map(|target| *target), refusal)));
            }
            HostEvent::Extension(SurfaceObservation::ReloadProbeFailed) => {
                self.apply_reload_probed(None);
            }
            HostEvent::Extension(SurfaceObservation::LookupStatus { text }) => {
                self.lookup_status = text;
                self.mark_dirty();
            }
            HostEvent::Extension(SurfaceObservation::OpenUsageOverlay) => {
                if self.pending_open_usage {
                    self.pending_open_usage = false;
                    self.info_overlay = None;
                    self.mark_dirty();
                }
            }
            HostEvent::Status {
                phase,
                queue_len: _,
                detail,
            } => {
                if self.clear_pending && phase != HostPhase::Clear {
                    self.clear_pending = false;
                }
                if self.phase != phase {
                    self.phase_started = Some(self.now());
                }
                self.phase = phase;
                if matches!(
                    phase,
                    HostPhase::Idle | HostPhase::Interrupted | HostPhase::Error
                ) {
                    self.prompt_row_awaiting_busy = false;
                }
                self.status_detail = detail.as_ref().map(|detail| {
                    crate::host_status_text::status_detail(
                        detail,
                        crate::notice_text::Frontend::Tui,
                    )
                });
                self.sync_root_turn_projection();
                self.mark_dirty();
            }
            HostEvent::QueueSnapshot { items, drain } => {
                self.replace_queue_snapshot(items, drain);
                self.retire_admitted_drafts();
            }
            HostEvent::ContextCapacityRecovery(recovery) => {
                self.capacity_recovery = recovery;
                self.mark_dirty();
            }
            HostEvent::ResumeSessions(entries) => {
                // Input is drained ahead of host observations, so the reply can
                // land after the Esc that dismissed the picker or after a newer
                // modal took input. Only an outstanding request opens it.
                if self.pending_open_resume && !self.a_modal_owns_input() {
                    self.pending_open_resume = false;
                    self.resume_menu = Some(ResumeMenu::open(
                        entries,
                        unix_now(),
                        self.time_zone.clone(),
                    ));
                    self.slash_open = false;
                    self.mark_dirty();
                }
            }
            HostEvent::TopologyNavigatorSnapshot(view) => {
                use plakous_host_protocol::topology::NodeStatus;
                self.agents_unavailable = None;
                // Count live descendants before the canvas adds presentation labels.
                let descendants = view
                    .nodes
                    .iter()
                    .filter(|node| {
                        node.parent_agent_id.is_some()
                            && !node.finished()
                            && matches!(
                                node.status,
                                NodeStatus::Pending | NodeStatus::Running | NodeStatus::Sealing
                            )
                    })
                    .count();
                if self.tree_live_descendants != descendants {
                    self.tree_live_descendants = descendants;
                    self.mark_dirty();
                }
                // The one place the protocol's typed tree becomes canvas rows.
                let snap = crate::tui::CanvasTree::from(&view);
                self.tree_epoch_state = Some(snap.state.clone());
                self.apply_topology_navigator_snapshot(snap);
            }
            HostEvent::TopologyNavigatorUnavailable { reason } => {
                self.tree_navigator = None;
                self.agents_unavailable = Some(reason);
                self.mark_dirty();
            }
            // Nothing requests call detail until the transcript v2 renderer.
            HostEvent::CallDetail(detail) => self.apply_call_detail(detail),
            HostEvent::TemplateReadiness(rows) => {
                self.templates = rows;
                self.mark_dirty();
            }
            HostEvent::Extension(SurfaceObservation::SettingsView(mut snapshot)) => {
                self.settings_error = None;
                snapshot.tui = self.preferences.clone();
                self.settings_snapshot = Some(snapshot.clone());
                if let Some(menu) = self.settings_menu.as_mut() {
                    menu.draft = snapshot.clone();
                } else if self.control_tab == Some(ControlTab::Settings)
                    || self.pending_open_settings
                {
                    self.settings_menu = Some(SettingsMenu::open(snapshot));
                }
                self.sync_render_prefs();
                if self.control_tab.is_none() && self.pending_open_settings {
                    self.control_tab = Some(ControlTab::Settings);
                    self.last_control_tab = ControlTab::Settings;
                }
                self.pending_open_settings = false;
                self.slash_open = false;
                self.mark_dirty();
            }
            HostEvent::Extension(SurfaceObservation::SettingsViewApplied(mut snapshot)) => {
                // Every row applies on change, so a confirmed write is
                // not an exit. The tab stays exactly where the operator left
                // it and only the persisted value moves under it.
                self.settings_error = None;
                snapshot.tui = self.preferences.clone();
                self.settings_snapshot = Some(snapshot.clone());
                if let Some(menu) = self.settings_menu.as_mut() {
                    menu.draft = snapshot;
                } else if self.control_tab == Some(ControlTab::Settings) {
                    self.settings_menu = Some(SettingsMenu::open(snapshot));
                }
                self.sync_render_prefs();
                self.mark_dirty();
            }
            HostEvent::Extension(SurfaceObservation::SettingsLoadFailed(message)) => {
                self.settings_menu = None;
                self.settings_snapshot = None;
                self.settings_error = Some(message);
                self.mark_dirty();
            }
            HostEvent::Extension(SurfaceObservation::SettingsSaveFailed(message)) => {
                self.settings_error = Some(message);
                self.mark_dirty();
            }
            HostEvent::DraftAccepted(action) => {
                self.accept_pending_draft(&action);
                self.mark_dirty();
            }
            HostEvent::DraftRejected(action) => {
                let action = self.remove_pending_draft(&action).unwrap_or(action);
                if let UserAction::Prompt(draft)
                | UserAction::FollowUp(draft)
                | UserAction::Steer(draft) = action
                {
                    self.restore_draft(draft);
                }
                self.mark_dirty();
            }
            HostEvent::DraftRestored(draft) => {
                self.accepted_drafts.clear();
                self.restore_draft(draft);
                self.mark_dirty();
            }
            HostEvent::QueueEditApplied { id } => {
                if self
                    .pending_queue_edit
                    .as_ref()
                    .is_some_and(|(pending_id, _)| pending_id == &id)
                {
                    let submitted = self.pending_queue_edit.take().map(|(_, text)| text);
                    let replaced = self
                        .queue_editing
                        .take()
                        .map(|edit| edit.replaced_draft)
                        .unwrap_or_default();
                    if submitted.as_ref() == Some(&self.input) {
                        self.set_input(replaced);
                    } else if !replaced.is_empty() {
                        self.restore_draft_text(replaced);
                    }
                }
                self.mark_dirty();
            }
            HostEvent::QueueEditRefused { id } => {
                if self
                    .pending_queue_edit
                    .as_ref()
                    .is_some_and(|(pending_id, _)| pending_id == &id)
                {
                    self.pending_queue_edit = None;
                }
                self.mark_dirty();
            }
            // The operator line the host sent with it is the visible record;
            // this observation is for machine faces.
            HostEvent::Recovered { .. } => {}
            HostEvent::ReattachReady => {
                self.request_reattach = true;
                self.should_quit = true;
                self.mark_dirty();
            }
            HostEvent::SessionOpening => {
                self.session_open_started = Some(self.now());
                self.session_open_ms = None;
                // The opening session's processes come as its own list; one
                // with none sends none.
                self.apply_process_snapshot(std::sync::Arc::new(Vec::new()));
                self.open_notes_from = Some(self.pane.blocks.len());
                self.mark_dirty();
            }
            HostEvent::ModelRequestQueued { slots } => {
                self.request_slot_wait = Some(slots);
                self.mark_dirty();
            }
            HostEvent::CredentialWait { account } => {
                self.credential_wait = account;
                self.request_slot_wait = None;
                self.mark_dirty();
            }
            HostEvent::ModelRequestStarted => {
                self.credential_wait = None;
                self.request_slot_wait = None;
                self.stream_discard_from = self.pane.blocks.len();
                self.model_request_started = Some(self.now());
                self.pane.assistant_stream_active = false;
                self.mark_dirty();
            }
            HostEvent::SessionOpened { session_open_ms } => {
                self.session_open_started = None;
                self.session_open_ms = session_open_ms;
                self.startup_pending = false;
                self.mark_dirty();
            }
            HostEvent::SetBusy(busy) => {
                // Every acknowledged draft's own row precedes a busy edge.
                self.accepted_drafts.clear();
                if self.busy && !busy {
                    self.composer_chrome.refresh_pending = true;
                    self.composer_chrome.sent_steers.clear();
                }
                self.busy = busy;
                self.prompt_row_awaiting_busy = false;
                if busy && self.turn_started.is_none() {
                    self.turn_started = Some(self.now());
                    self.phase_started = Some(self.now());
                }
                if !busy {
                    self.queued_steers_retired = 0;
                    self.turn_started = None;
                    self.phase_started = None;
                    self.model_request_started = None;
                    self.request_slot_wait = None;
                    self.credential_wait = None;
                    self.retire_unresolved_approval();
                }
                if busy {
                    self.session_open_ms = None;
                }
                if !busy {
                    // The turn settled: a further Ctrl+C is no longer an escalation.
                    self.cancel_pending = false;
                    self.quit_arm = None;
                }
                if busy && self.phase == HostPhase::Idle {
                    self.phase = HostPhase::Running;
                } else if !busy && self.phase == HostPhase::Running {
                    self.phase = HostPhase::Idle;
                }
                self.sync_root_turn_projection();
                self.mark_dirty();
            }
            HostEvent::PromptHistorySnapshot(prompts) => {
                let keep_from = prompts.len().saturating_sub(PROMPT_HISTORY_LIMIT);
                self.recovered_drafts
                    .retain(|draft| !prompts.contains(draft));
                self.prompt_history = prompts
                    .into_iter()
                    .skip(keep_from)
                    .chain(self.recovered_drafts.iter().cloned())
                    .collect();
                self.history_browse = None;
                self.mark_dirty();
            }
            // the provider's reasoning visibility policy already
            // gated these host-side. The renderer only folds them (§5.6).
            HostEvent::ApprovalRequired {
                approval_id,
                call_id: _,
                tool_name,
                subject,
                prompt,
                expires_at_ms: _,
            } => {
                self.open_approval = Some(approval_id.clone());
                self.push_block(ScrollBlock::Approval {
                    approval_id,
                    tool_name,
                    subject,
                    prompt,
                    resolution: None,
                });
            }
            HostEvent::ApprovalResolved {
                approval_id,
                resolution,
            } => {
                if self.open_approval.as_deref() == Some(approval_id.as_str()) {
                    self.open_approval = None;
                }
                let Some(idx) = self.pane.blocks.iter().rposition(|block| {
                    matches!(block, ScrollBlock::Approval { approval_id: id, .. } if *id == approval_id)
                }) else {
                    return;
                };
                if let ScrollBlock::Approval {
                    resolution: existing,
                    ..
                } = &mut self.pane.blocks[idx]
                {
                    *existing = Some(resolution);
                }
                self.mark_block_changed(idx);
            }
            HostEvent::JobStarted { .. }
            | HostEvent::JobPromoted { .. }
            | HostEvent::JobTerminal { .. } => {}
            HostEvent::PendingContinuation {
                results,
                questions,
                process_exits,
                auto_continue_paused,
            } => {
                self.pending_continuation = plakous_host_protocol::PendingContinuation {
                    results,
                    questions,
                    process_exits,
                    auto_continue_paused,
                };
                self.mark_dirty();
            }
            HostEvent::LastUserPromptAt(sent) => {
                let (now, now_unix_ms) = (self.now(), self.unix_now_ms());
                self.seed_last_prompt_at(sent, now, now_unix_ms);
            }
            HostEvent::Processes { views, .. } => {
                self.apply_process_snapshot(std::sync::Arc::new(views));
            }
            HostEvent::SteerAck { state, .. } => {
                match state.as_str() {
                    "queued" => {
                        let text = self.retire_accepted_steer();
                        if text.is_some() {
                            self.queued_steers_retired += 1;
                        }
                        self.composer_chrome.sent_steers.push_back(text);
                    }
                    "applied" => {
                        if self.queued_steers_retired == 0 {
                            let _ = self.retire_accepted_steer();
                        } else {
                            self.queued_steers_retired -= 1;
                        }
                        let _ = self.composer_chrome.sent_steers.pop_front();
                    }
                    _ => {}
                }
                self.mark_dirty();
            }
        }
    }

    pub(super) fn poll_usage_decoration(&mut self) {
        if let Some((limits, notes)) = self
            .usage_decoration
            .as_mut()
            .and_then(crate::config::UsageDecorationReader::poll)
        {
            self.usage_limits = limits;
            self.refresh_failure_notes = notes;
            self.mark_dirty();
        }
    }
}
