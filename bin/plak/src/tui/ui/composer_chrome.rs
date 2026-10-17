//! Border HUD, width-budgeted hints, and composer-region pointer affordances.

use super::{
    App, Block, BorderType, Borders, Clear, Frame, Line, MouseEvent, Paragraph, Rect, ScrollBlock,
    Span, Style, UserAction, format_context_meter, format_status_elapsed,
};
use crate::tui::presentation::ActivityKind;
use crate::tui::{ProfileRow, SurfaceIntent};
use ratatui::layout::Position;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) struct ComposerChrome {
    pointer: Option<Position>,
    pub(super) profile: Option<ProfileRow>,
    pub(super) profile_hit: Option<Rect>,
    pub(super) sent_steers: usize,
    pub(super) refresh_pending: bool,
}

impl Default for ComposerChrome {
    fn default() -> Self {
        Self {
            pointer: None,
            profile: None,
            profile_hit: None,
            sent_steers: 0,
            refresh_pending: true,
        }
    }
}

pub(super) fn activity_label(name: &str, kind: ActivityKind, subject: Option<&str>) -> String {
    if crate::tui::blocks::subject_tool_verb(name, kind, subject)
        == crate::tui::blocks::ToolVerb::Mcp
        && let Some(call) = crate::tui::presentation::McpCall::parse(name)
    {
        return format!("calling mcp:{}", call.server);
    }
    crate::tui::activity::hud_verb(name, kind, subject).to_owned()
}

pub(super) fn clip_text(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let cells = grapheme.width();
        if used + cells > width {
            break;
        }
        result.push_str(grapheme);
        used += cells;
    }
    result
}

/// How long ago the last prompt was sent, coarsely: seconds, then whole
/// minutes, then the shared ladder's hours and minutes.
pub(super) fn prompt_age(age: std::time::Duration) -> String {
    let secs = age.as_secs();
    match secs {
        60..3600 => format!("{}m ago", secs / 60),
        _ => format!("{} ago", crate::tui::blocks::format_duration_secs(secs)),
    }
}

/// The shortest path tail worth showing; below it the path is dropped.
const MIN_PATH_TAIL: usize = 4;

/// ` path · branch ` fitted to `width` cells: the path keeps its tail behind
/// `…` before it is dropped, and the branch is clipped last. Empty when not
/// even a clipped branch fits.
pub(super) fn workspace_border_text(label: &str, width: usize) -> String {
    // Git branch names cannot contain spaces, so the last ` · ` separates them.
    let (path, branch) = match label.rsplit_once(" · ") {
        Some((path, branch)) => (path, Some(branch)),
        None => (label, None),
    };
    let framed = |path: &str| match branch {
        Some(branch) if path.is_empty() => format!(" {branch} "),
        Some(branch) => format!(" {path} · {branch} "),
        None if path.is_empty() => String::new(),
        None => format!(" {path} "),
    };
    let full = framed(path);
    if full.width() <= width {
        return full;
    }
    let fixed = framed("x").width() - 1;
    let tail_budget = width.saturating_sub(fixed + 1);
    if tail_budget >= MIN_PATH_TAIL {
        return framed(&format!("…{}", tail_text(path, tail_budget)));
    }
    let Some(branch) = branch else {
        return String::new();
    };
    let bare = framed("");
    if bare.width() <= width {
        return bare;
    }
    if width < 4 {
        return String::new();
    }
    format!(" {}… ", clip_text(branch, width - 3))
}

/// The longest grapheme tail of `text` that fits `width` cells.
fn tail_text(text: &str, width: usize) -> &str {
    let mut used = 0;
    let mut start = text.len();
    for (index, grapheme) in text.grapheme_indices(true).rev() {
        used += grapheme.width();
        if used > width {
            break;
        }
        start = index;
    }
    &text[start..]
}

impl App {
    pub(super) fn chrome_hovered(&self, rect: Rect) -> bool {
        self.composer_chrome
            .pointer
            .is_some_and(|point| rect.contains(point))
    }

    pub(super) fn track_chrome_pointer(&mut self, mouse: MouseEvent) {
        let point = Position::new(mouse.column, mouse.row);
        if self.composer_chrome.pointer != Some(point) {
            let before = self.chrome_hover_target(self.composer_chrome.pointer);
            self.composer_chrome.pointer = Some(point);
            let after = self.chrome_hover_target(Some(point));
            if before != after {
                self.mark_dirty();
            }
        }
    }

    fn chrome_hover_target(&self, point: Option<Position>) -> Option<Rect> {
        let point = point?;
        if self.destructive_confirm.is_some() {
            return self
                .destructive_confirm_hits()
                .find(|rect| rect.contains(point));
        }
        if self.control_tab.is_some() || self.activity_tab.is_some() || self.info_overlay.is_some()
        {
            return None;
        }
        [
            self.status_agents_hit,
            self.status_processes_hit,
            self.composer_chrome.profile_hit,
        ]
        .into_iter()
        .flatten()
        .chain(
            self.queue_button_hits
                .iter()
                .flat_map(|hit| [hit.now, hit.drop]),
        )
        .chain(self.transcript_area.into_iter().flat_map(|area| {
            self.tool_hit_rows
                .iter()
                .map(move |(_, y)| Rect::new(area.x, *y, area.width, 1))
        }))
        .find(|rect| rect.contains(point))
    }

    pub(super) fn maybe_refresh_composer_chrome(
        &mut self,
        actions: Option<&crate::tui::UserActionSender>,
    ) {
        if !self.composer_chrome.refresh_pending {
            return;
        }
        if let Some(sender) = actions
            && sender
                .send(UserAction::Extension(SurfaceIntent::RefreshComposerChrome))
                .is_ok()
        {
            self.composer_chrome.refresh_pending = false;
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "border budget and painted hit geometry share one projection"
    )]
    pub(super) fn draw_composer_top(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.status_agents_hit = None;
        self.status_processes_hit = None;
        self.composer_chrome.profile_hit = None;
        if area.width < 3 || area.height == 0 {
            return;
        }
        let available = usize::from(area.width.saturating_sub(2));
        let meter = format_context_meter(
            self.context_usage.display_percent,
            self.context_usage.total_window,
        );
        let meter = if available < 40 && meter == "awaiting context" {
            "awaiting…".to_owned()
        } else {
            meter
        };
        let meter = format!(" {meter} ");
        let label = self.turn_status_label();
        let pending_label = self.idle_pending_label();
        let mut state = String::new();
        if let Some(label) = &label {
            let glyph = if label == "approval needed" {
                if self.render_prefs.reduced_motion || self.status_spinner_frame.is_multiple_of(2) {
                    "◆"
                } else {
                    "◇"
                }
            } else if pending_label.as_ref() == Some(label) {
                // Idle pending chrome is a steady diamond, not a spinner.
                "◆"
            } else if self.busy && !label.ends_with('…') && !label.starts_with("error:") {
                self.status_spinner()
            } else {
                ""
            };
            state = format!("{glyph} {label}").trim_start().to_owned();
            if matches!(
                label.as_str(),
                "thinking"
                    | "compacting"
                    | "waiting for model"
                    | "running command"
                    | "editing"
                    | "reading"
                    | "searching"
                    | "waiting"
                    | "waiting on agents"
                    | "working"
            ) && let Some(start) = self.hud_activity_started()
            {
                state.push_str(&format!(
                    " {}",
                    format_status_elapsed(self.now().saturating_duration_since(start))
                ));
            }
        }
        // Each counter is its lead (separator and motion glyph) and its count
        // text; only the count is the button.
        let [agents, processes] = self.hud_counts();
        let counter_width = |counter: &(String, String)| counter.0.width() + counter.1.width();
        let counts_width =
            agents.as_ref().map_or(0, counter_width) + processes.as_ref().map_or(0, counter_width);
        let profile_id = self
            .composer_chrome
            .profile
            .as_ref()
            .map_or("custom", |profile| profile.id.as_str());
        let profile = format!(" {profile_id} ");
        // Meter and live facts win over the profile on a narrow border.
        let profile_width = if meter.width()
            + state.width()
            + counts_width
            + 1
            + profile.width()
            + 3
            <= available
        {
            profile.width()
        } else {
            0
        };
        let left_budget =
            available.saturating_sub(profile_width + usize::from(profile_width > 0) + 1);
        let meter = clip_text(&meter, left_budget);
        let state_budget = left_budget.saturating_sub(meter.width() + counts_width);
        let state = clip_text(&state, state_budget);
        let state_style = if label
            .as_ref()
            .is_some_and(|label| label.starts_with("error:"))
        {
            self.theme.error
        } else {
            self.theme.busy
        };
        let mut left = vec![
            Span::styled(meter.clone(), self.theme.chrome),
            Span::styled(state.clone(), state_style),
        ];
        let mut used = meter.width() + state.width();
        let pointer = self.composer_chrome.pointer;
        for (counter, hit_slot) in [
            (agents, &mut self.status_agents_hit),
            (processes, &mut self.status_processes_hit),
        ] {
            let Some((lead, count)) = counter else {
                continue;
            };
            if used + lead.width() + count.width() > left_budget {
                continue;
            }
            let hit = Rect::new(
                area.x + 1 + u16::try_from(used + lead.width()).unwrap_or(0),
                area.y,
                u16::try_from(count.width()).unwrap_or(0),
                1,
            );
            *hit_slot = Some(hit);
            used += lead.width() + count.width();
            left.push(Span::styled(lead, self.theme.chrome));
            left.push(Span::styled(
                count,
                if pointer.is_some_and(|point| hit.contains(point)) {
                    self.theme.slash_selected
                } else {
                    self.theme.chrome
                },
            ));
        }
        if left
            .iter()
            .rev()
            .find(|span| !span.content.is_empty())
            .is_some_and(|span| !span.content.ends_with(' '))
        {
            left.push(Span::styled(" ", self.theme.chrome));
        }
        let left_width = left.iter().map(Span::width).sum::<usize>();
        frame.render_widget(
            Paragraph::new(Line::from(left)).style(Style::default().bg(self.theme.app_bg)),
            Rect::new(
                area.x + 1,
                area.y,
                u16::try_from(left_width).unwrap_or(0),
                1,
            ),
        );
        if profile_width > 0 {
            let hit = Rect::new(
                area.right() - 1 - u16::try_from(profile_width).unwrap_or(0),
                area.y,
                u16::try_from(profile_width).unwrap_or(0),
                1,
            );
            self.composer_chrome.profile_hit = Some(hit);
            // Hover-only information: no button background and no click action.
            frame.render_widget(
                Paragraph::new(Span::styled(profile, self.theme.chrome))
                    .style(Style::default().bg(self.theme.app_bg)),
                hit,
            );
        }
    }

    /// The HUD's live agents and background processes counts, each as
    /// ` · <spinner> N agents`, when nonzero.
    /// The HUD's agents and processes counters, each as its lead (separator
    /// and motion glyph) and its count text.
    pub(super) fn hud_counts(&self) -> [Option<(String, String)>; 2] {
        let agents = (self.tree_live_descendants > 0).then(|| {
            (
                format!(" · {} ", self.counter_spinner(2)),
                format!(
                    "{} {}",
                    self.tree_live_descendants,
                    if self.tree_live_descendants == 1 {
                        "agent"
                    } else {
                        "agents"
                    }
                ),
            )
        });
        let processes = (self.background_processes > 0).then(|| {
            (
                format!(
                    " · {} ",
                    if self.render_prefs.reduced_motion {
                        "⠴"
                    } else {
                        self.counter_spinner(5)
                    }
                ),
                format!(
                    "{} {}",
                    self.background_processes,
                    if self.background_processes == 1 {
                        "process"
                    } else {
                        "processes"
                    }
                ),
            )
        });
        [agents, processes]
    }

    fn hud_activity_started(&self) -> Option<super::Instant> {
        if self.turn_status_label().as_deref() == Some("waiting for model") {
            return self.model_request_started;
        }
        let active_tool = self
            .root_pane().blocks
            .iter()
            .rev()
            .take(super::HUD_SCAN_BLOCKS)
            .find(|block| {
                matches!(block, ScrollBlock::Tool { state, .. } if super::tool_is_running(state))
            });
        if let Some(ScrollBlock::Tool { call_id, name, .. }) = active_tool {
            return self
                .root_pane()
                .tool_started
                .get(call_id.as_ref().unwrap_or(name))
                .copied()
                .or(self.phase_started);
        }
        self.root_pane()
            .thinking_started
            .values()
            .max()
            .copied()
            .or(self.phase_started)
            .or(self.turn_started)
    }

    pub(super) fn counter_spinner(&self, offset: usize) -> &'static str {
        if self.render_prefs.reduced_motion {
            return "⠿";
        }
        ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"][(self.status_spinner_frame + offset) % 8]
    }

    /// Paints the workspace facts right-aligned into the composer's bottom
    /// border, styled like the profile id in the top border.
    pub(super) fn draw_composer_bottom(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width < 3 || area.height < 2 {
            return;
        }
        // Attachment chips lead the border from the left and take the room
        // they need first: they say what the next message sends. The
        // workspace facts keep their right-aligned place in what is left.
        let chips = self.attachment_chips(usize::from(area.width.saturating_sub(6)));
        let chips_room = match chips.width() {
            0 => 0,
            chips_width => chips_width.saturating_add(1),
        };
        // One border cell stays visible after the corner.
        let text = workspace_border_text(
            &self.workspace_chrome,
            usize::from(area.width.saturating_sub(3)).saturating_sub(chips_room),
        );
        let width = u16::try_from(text.width()).unwrap_or(0);
        if !chips.is_empty() {
            let chips_width = u16::try_from(chips.width()).unwrap_or(0);
            frame.render_widget(
                Paragraph::new(Span::styled(chips, self.theme.user_label))
                    .style(Style::default().bg(self.theme.app_bg)),
                Rect::new(area.x + 2, area.bottom() - 1, chips_width, 1),
            );
        }
        if width == 0 {
            return;
        }
        frame.render_widget(
            Paragraph::new(Span::styled(text, self.theme.chrome))
                .style(Style::default().bg(self.theme.app_bg)),
            Rect::new(area.right() - 1 - width, area.bottom() - 1, width, 1),
        );
    }

    /// Seeds the idle clock from the host's stored time of the last prompt
    /// after an attach. A prompt sent from this surface since is newer and
    /// stays; a stored time ahead of the local clock reads as just sent.
    pub(super) fn seed_last_prompt_at(
        &mut self,
        sent_unix_ms: Option<u64>,
        now: super::Instant,
        now_unix_ms: u64,
    ) {
        let Some(sent_unix_ms) = sent_unix_ms else {
            return;
        };
        let age = std::time::Duration::from_millis(now_unix_ms.saturating_sub(sent_unix_ms));
        let Some(stored) = now.checked_sub(age) else {
            return;
        };
        if self.last_prompt_at.is_none_or(|local| local < stored) {
            self.last_prompt_at = Some(stored);
            self.mark_dirty();
        }
    }

    /// `last user prompt · 25m ago` while the root is idle after a prompt
    /// this session, else `None` (spec 02 §5.4).
    pub(super) fn idle_prompt_clock(&self, now: super::Instant) -> Option<String> {
        if self.busy || self.turn_status_label().is_some() {
            return None;
        }
        let sent = self.last_prompt_at?;
        Some(prompt_age(now.saturating_duration_since(sent)))
    }

    pub(super) fn draw_composer_hint(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let available = usize::from(area.width.saturating_sub(2));
        let full = if self.newer_build_installed {
            "/ commands · Ctrl+X menu · ? keybinds · new build installed · /reload"
        } else {
            "/ commands · Ctrl+X menu · ? keybinds"
        };
        let keys = if self.newer_build_installed {
            "/ · Ctrl+X · ? · /reload"
        } else {
            "/ · Ctrl+X · ?"
        };
        let stop = if self.busy { " · Esc stop" } else { "" };
        let idle_clock = self.idle_prompt_clock(self.now());
        self.idle_clock_painted.clone_from(&idle_clock);
        // The idle clock gives up its words before the hint labels do, and
        // itself before the keys.
        let right = match idle_clock {
            Some(age) => {
                let candidates = [format!("last user prompt · {age}"), age];
                [full, keys]
                    .into_iter()
                    .find_map(|hints| {
                        candidates
                            .iter()
                            .find(|text| hints.width() + text.width() < available)
                    })
                    .cloned()
                    .unwrap_or_default()
            }
            None => self
                .turn_started
                .filter(|_| self.busy)
                .map_or_else(String::new, |start| {
                    format_status_elapsed(self.now().saturating_duration_since(start))
                }),
        };
        let hints = if full.width() + stop.width() + right.width() + 2 <= available {
            full
        } else {
            keys
        };
        let right_width = right.width().min(available);
        let left_budget = available.saturating_sub(right_width + usize::from(right_width > 0));
        let stop = if stop.width() <= left_budget {
            stop
        } else {
            ""
        };
        let hints = clip_text(hints, left_budget.saturating_sub(stop.width()));
        let stop = if hints.is_empty() {
            stop.strip_prefix(" · ").unwrap_or(stop)
        } else {
            stop
        };
        let mut spans = vec![
            Span::styled(
                hints,
                if self.newer_build_installed {
                    self.theme.chrome
                } else {
                    self.theme.chrome_dim
                },
            ),
            Span::styled(stop, self.theme.error),
        ];
        let used = spans.iter().map(Span::width).sum::<usize>();
        spans.push(Span::raw(
            " ".repeat(available.saturating_sub(used + right_width)),
        ));
        spans.push(Span::styled(
            clip_text(&right, right_width),
            self.theme.chrome_dim,
        ));
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1),
        );
    }

    pub(super) fn draw_profile_hover(&self, frame: &mut Frame<'_>) {
        let Some(hit) = self
            .composer_chrome
            .profile_hit
            .filter(|hit| self.chrome_hovered(*hit))
        else {
            return;
        };
        if self.control_tab.is_some()
            || self.activity_tab.is_some()
            || self.info_overlay.is_some()
            || self.destructive_confirm.is_some()
        {
            return;
        }
        let mut lines = Vec::new();
        if let Some(profile) = &self.composer_chrome.profile {
            for role in std::iter::once(&profile.root).chain(&profile.cards) {
                lines.push(Line::from(Span::styled(
                    format!(
                        "{}: {} · {}{}",
                        role.label,
                        role.model,
                        role.effort,
                        if role.fast { " · Fast" } else { "" }
                    ),
                    self.theme.slash_item,
                )));
            }
        } else {
            lines.push(Line::from(Span::styled(
                format!(
                    "root: {} · {}",
                    self.model.as_deref().unwrap_or("not attached"),
                    self.effort.as_deref().unwrap_or("default")
                ),
                self.theme.slash_item,
            )));
        }
        lines.push(Line::from(Span::styled(
            "Menu → Agents",
            self.theme.slash_item_help,
        )));
        let width = u16::try_from(lines.iter().map(Line::width).max().unwrap_or(0) + 4)
            .unwrap_or(u16::MAX)
            .min(frame.area().width);
        let height = u16::try_from(lines.len() + 2)
            .unwrap_or(u16::MAX)
            .min(hit.y.saturating_sub(frame.area().y));
        if height < 3 {
            return;
        }
        let popup = Rect::new(
            frame.area().right().saturating_sub(width),
            hit.y.saturating_sub(height),
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.theme.menu_border)
            .style(Style::default().bg(self.theme.menu_bg));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        frame.render_widget(Paragraph::new(lines), inner);
    }
}
