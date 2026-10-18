//! Host event application.

use super::{
    App, ControlTab, DisplayMode, Duration, HostEvent, PROMPT_HISTORY_LIMIT, ResumeMenu,
    ScrollBlock, SettingsMenu, UserAction, blocks, format_tool_elapsed,
    terminal_state_keeping_duration, theme_for, tool_is_running, unix_now, unknown_context_usage,
};
use crate::tui::SurfaceObservation;

impl App {
    /// Marks the assistant text and reasoning the failed request streamed:
    /// the trailing blocks of this model call, which a replay only follows
    /// before any tool call began.
    fn discard_streamed_assistant(&mut self) {
        for index in (0..self.pane.blocks.len()).rev() {
            let (text, thinking) = match &mut self.pane.blocks[index] {
                ScrollBlock::Assistant(text) => (std::mem::take(text), false),
                ScrollBlock::Thinking { text, .. } => (std::mem::take(text), true),
                ScrollBlock::Discarded { .. } => continue,
                _ => break,
            };
            self.pane.blocks[index] = ScrollBlock::Discarded { text, thinking };
            self.mark_block_changed(index);
        }
        self.pane.assistant_stream_active = false;
    }

    /// Apply a host event (status / transcript / structured blocks).
    pub(crate) fn apply_host_event(&mut self, event: HostEvent) {
        self.apply_host_event_state(event);
        // An open pop-up whose target stopped being active closes.
        self.retire_stale_confirm();
    }

    fn apply_host_event_state(&mut self, event: HostEvent) {
        if self.ends_agent_view(&event) {
            self.close_agent_view();
            // Its target belonged to the session that was attached.
            if self.destructive_confirm.take().is_some() {
                self.mark_dirty();
            }
        }
        if let HostEvent::ChildLive {
            agent_id,
            turn_id,
            op,
        } = event
        {
            self.semantic_revision = self.semantic_revision.saturating_add(1);
            if self.shows_live_turn(&agent_id, &turn_id)
                && let Some(op) = child_transcript_op(*op)
            {
                self.apply_live_op(op);
            }
            return;
        }
        if let HostEvent::AgentTranscript(transcript) = event {
            self.semantic_revision = self.semantic_revision.saturating_add(1);
            self.replace_agent_rows(
                &transcript.agent_id,
                transcript.rows,
                transcript.live_turn_id,
                transcript.note,
            );
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
                        self.mark_block_changed(index);
                        return;
                    }
                    self.push_block(block);
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
                    self.pane.assistant_stream_active = true;
                } else if let Some(ScrollBlock::Assistant(text)) = self.pane.blocks.last_mut() {
                    text.push_str(&delta);
                } else {
                    self.pane.blocks.push(ScrollBlock::Assistant(delta));
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
                mut state,
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
                    let measured = self.pane.tool_started.remove(&timing_key).map(|started| {
                        format_tool_elapsed(self.now().saturating_duration_since(started))
                    });
                    let timing = duration_ms
                        .map(|ms| format_tool_elapsed(Duration::from_millis(ms)))
                        .or(measured);
                    if let Some(timing) = timing {
                        if state == "done" || state == "overflow" {
                            state = format!("{state}:{timing}");
                        } else {
                            // Every terminal keeps its elapsed, `cancelled` and
                            // `rejected:*` included.
                            state.push_str(" · ");
                            state.push_str(&timing);
                        }
                    }
                }
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
                self.push_block(ScrollBlock::Tool {
                    call_id,
                    parent_call_id,
                    cell_id,
                    kind,
                    name,
                    state,
                    subject,
                    detail,
                    mode: DisplayMode::Collapsed,
                    manual: false,
                    group_open: false,
                    wait,
                    started_at_ms,
                    finished_at_ms,
                    view: blocks::ItemView::default(),
                });
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
                    self.push_block(ScrollBlock::Thinking {
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
            HostEvent::ModelRetry(retry) => {
                if retry.output_discarded {
                    self.discard_streamed_assistant();
                }
                self.pending_retry = Some((retry, self.now()));
                self.mark_dirty();
            }
            HostEvent::ToolInvocationOrigin { .. }
            | HostEvent::PromptAdmitted(_)
            | HostEvent::PromptStarted(_)
            | HostEvent::PromptCorrelationReset
            | HostEvent::IntentStarted(_) => {}
            HostEvent::PresentationSnapshot(snapshot) => {
                if let Err(error) = snapshot.validate() {
                    self.pane.blocks.push(ScrollBlock::Error(format!(
                        "invalid presentation snapshot refused: {error:?}"
                    )));
                    self.mark_transcript_replaced();
                    return;
                }
                self.pane.assistant_stream_active = false;
                self.restore_tip_clocks(&snapshot.rows);
                let full_window =
                    snapshot.rows.len() == crate::tui::presentation::MAX_SNAPSHOT_ROWS;
                let mut blocks = blocks::blocks_from_rows(snapshot.rows);
                self.restore_opening_note(&mut blocks, full_window);
                for note in self.take_open_notes() {
                    let already = blocks.iter().any(|block| match (block, &note) {
                        (ScrollBlock::System(have), ScrollBlock::System(want))
                        | (ScrollBlock::Error(have), ScrollBlock::Error(want)) => have == want,
                        _ => false,
                    });
                    if !already {
                        blocks.push(note);
                    }
                }
                let old = std::mem::replace(&mut self.pane.blocks, blocks);
                self.carry_item_state(&old);
                self.pane.scroll_from_bottom = 0;
                self.mark_transcript_replaced();
                self.warm_transcript_projection();
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
            | HostEvent::ReasoningSealed { .. }) => self.apply_live_op(event),
            HostEvent::UserMessage { text, sent_at_ms } => {
                self.push_block(ScrollBlock::User { text, sent_at_ms });
            }
            HostEvent::ModelRefusal { detail } => {
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
                        self.last_turn = None;
                        self.last_prompt_at = None;
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
                self.clear_pending = false;
                self.last_turn = None;
                self.pane.blocks.clear();
                self.pane.assistant_stream_active = false;
                self.open_approval = None;
                self.pane.scroll_from_bottom = 0;
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
            HostEvent::UsageLimits(mut limits) => {
                self.refresh_failure_notes = crate::product_subscription_usage::annotate_limits(
                    &mut limits,
                    self.telemetry_paths.as_ref(),
                    self.time_zone,
                );
                self.usage_limits = limits;
                self.mark_dirty();
            }
            HostEvent::SessionInfo(info) => {
                self.session_info = info;
                self.mark_dirty();
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
                self.mark_dirty();
            }
            HostEvent::Extension(SurfaceObservation::McpOverview(body)) => {
                self.info_overlay = Some(("mcp".to_owned(), body));
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
                if self.clear_pending && phase != "clear" {
                    self.clear_pending = false;
                }
                if self.phase != phase {
                    self.phase_started = Some(self.now());
                }
                self.phase = phase;
                self.status_detail = detail;
                self.sync_root_turn_projection();
                self.mark_dirty();
            }
            HostEvent::QueueSnapshot { items, drain } => {
                self.replace_queue_snapshot(items, drain);
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
            // Child operations never reach the root pane.
            HostEvent::ChildLive { .. } | HostEvent::AgentTranscript(_) => {}
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
            HostEvent::Extension(SurfaceObservation::SettingsSnapshot(snapshot)) => {
                self.settings_error = None;
                self.persisted_theme = snapshot.theme;
                self.theme = theme_for(snapshot.theme);
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
            HostEvent::Extension(SurfaceObservation::SettingsApplied(snapshot)) => {
                // Every row applies on change, so a confirmed write is
                // not an exit. The tab stays exactly where the operator left
                // it and only the persisted value moves under it.
                self.settings_error = None;
                self.persisted_theme = snapshot.theme;
                self.theme = theme_for(snapshot.theme);
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
                let _ = self.remove_pending_draft(&action);
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
                self.open_notes_from = Some(self.pane.blocks.len());
                self.mark_dirty();
            }
            HostEvent::ModelRequestQueued { slots } => {
                self.request_slot_wait = Some(slots);
                self.mark_dirty();
            }
            HostEvent::ModelRequestStarted => {
                self.request_slot_wait = None;
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
                if self.busy && !busy {
                    self.composer_chrome.refresh_pending = true;
                    self.composer_chrome.sent_steers = 0;
                }
                self.busy = busy;
                if busy && self.turn_started.is_none() {
                    self.turn_started = Some(self.now());
                    self.phase_started = Some(self.now());
                }
                if !busy {
                    self.turn_started = None;
                    self.phase_started = None;
                    self.model_request_started = None;
                    self.request_slot_wait = None;
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
                if busy && self.phase == "idle" {
                    "running".clone_into(&mut self.phase);
                } else if !busy && self.phase == "running" {
                    "idle".clone_into(&mut self.phase);
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
                tree_paused,
            } => {
                self.pending_continuation = plakous_host_protocol::PendingContinuation {
                    results,
                    questions,
                    process_exits,
                    auto_continue_paused,
                    tree_paused,
                };
                self.mark_dirty();
            }
            HostEvent::LastUserPromptAt(sent) => {
                let (now, now_unix_ms) = (self.now(), self.unix_now_ms());
                self.seed_last_prompt_at(sent, now, now_unix_ms);
            }
            HostEvent::SteerAck { state, .. } => {
                match state.as_str() {
                    "queued" => self.composer_chrome.sent_steers += 1,
                    "applied" => {
                        self.composer_chrome.sent_steers =
                            self.composer_chrome.sent_steers.saturating_sub(1);
                    }
                    _ => {}
                }
                self.mark_dirty();
            }
        }
    }
}

/// A child turn's op in the TUI's vocabulary, when it is a transcript op.
/// Usage and busy ops describe the child's own turn, not the main view, and
/// are dropped.
fn child_transcript_op(
    op: plakous_host_protocol::SessionObservation<plakous_host_protocol::Core>,
) -> Option<HostEvent> {
    use plakous_host_protocol::SessionObservation as Op;
    Some(match op {
        Op::Row(row) => HostEvent::Row(row),
        Op::AssistantDelta(delta) => HostEvent::AssistantDelta(delta),
        Op::AssistantDone => HostEvent::AssistantDone,
        Op::ToolLine {
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
        } => HostEvent::ToolLine {
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
        },
        Op::ReasoningDelta { location_key, text } => {
            HostEvent::ReasoningDelta { location_key, text }
        }
        Op::ReasoningSealed { location_key } => HostEvent::ReasoningSealed { location_key },
        _ => return None,
    })
}
