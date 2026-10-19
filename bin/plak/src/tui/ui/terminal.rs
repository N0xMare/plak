//! Terminal lifecycle, event loop, and action dispatch.

use super::{
    App, BeginSynchronizedUpdate, ControlTab, DefaultTerminal, DisableBracketedPaste,
    DisableFocusChange, DisableMouseCapture, Duration, EnableBracketedPaste, EnableFocusChange,
    EnableMouseCapture, EndSynchronizedUpdate, Event, FrameWork, HostEvent, Instant,
    KeyboardEnhancementFlags, MAX_HOST_EVENTS_PER_FRAME, MAX_INPUT_DRAIN_PER_FRAME, Once, Path,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags, RedrawSchedule, ScrollBlock,
    SettingsMenu, ShowCursor, TuiError, TuiHandoff, TuiLaunch, TuiOutcome, UserAction, event,
    local_echo_line, redraw, stdout,
};
use crate::tui::SurfaceIntent;
#[cfg(feature = "test-hooks")]
use crate::tui::ui::pilot_sidecar::PilotSidecar;
use crossterm::ExecutableCommand;
use crossterm::tty::IsTty;

pub(super) fn from_io(error: &std::io::Error) -> TuiError {
    TuiError::Io(error.to_string())
}
#[cfg(test)]
pub(super) fn combine_terminal_cleanup(
    result: Result<TuiOutcome, TuiError>,
    cleanup_errors: &[String],
) -> Result<TuiOutcome, TuiError> {
    if cleanup_errors.is_empty() {
        return result;
    }
    let cleanup = cleanup_errors.join("; ");
    match result {
        Ok(_) => Err(TuiError::Io(format!("terminal cleanup failed: {cleanup}"))),
        Err(error) => Err(TuiError::Io(format!(
            "{error}; terminal cleanup also failed: {cleanup}"
        ))),
    }
}

/// Best-effort OSC 52 clipboard (works over many SSH terminals).
pub(super) fn copy_text_osc52(text: &str) {
    use std::io::Write as _;
    let Some(seq) = osc52_sequence(text) else {
        return;
    };
    let mut terminal = stdout();
    let _ = terminal.write_all(seq.as_bytes());
    let _ = terminal.flush();
}

/// Pure PTY-test hook for the exact bounded OSC52 payload emitted by copy.
pub(super) fn osc52_sequence(text: &str) -> Option<String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;
    let encoded = B64.encode(text.as_bytes());
    (encoded.len() <= 64 * 1024).then(|| format!("\x1b]52;c;{encoded}\x07"))
}

/// Restores every process-global terminal mode even when the UI unwinds.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent terminal modes must each restore only when successfully enabled"
)]
pub(super) struct TerminalRestoration {
    pub(super) mouse_captured: bool,
    pub(super) bracketed_paste: bool,
    pub(super) keyboard_pushed: bool,
    pub(super) focus_change: bool,
    pub(super) armed: bool,
    #[cfg(test)]
    pub(super) test_hook: Option<Box<dyn FnMut() + Send>>,
}

impl TerminalRestoration {
    pub(super) fn new() -> Self {
        Self {
            mouse_captured: false,
            bracketed_paste: false,
            keyboard_pushed: false,
            focus_change: false,
            armed: true,
            #[cfg(test)]
            test_hook: None,
        }
    }

    #[cfg(test)]
    pub(super) fn with_test_hook(hook: impl FnMut() + Send + 'static) -> Self {
        Self {
            test_hook: Some(Box::new(hook)),
            ..Self::new()
        }
    }

    pub(super) fn restore(&mut self) -> Vec<String> {
        if !self.armed {
            return Vec::new();
        }
        self.armed = false;
        #[cfg(test)]
        if let Some(hook) = &mut self.test_hook {
            hook();
            return Vec::new();
        }

        let mut errors = Vec::new();
        // Held for the whole restore, so a forced exit from the signal
        // watcher never interleaves its own restore with this one.
        let mut held = TERMINAL_HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *held = false;
        crate::lookup_status::tui_active(false);
        if self.bracketed_paste
            && let Err(error) = stdout().execute(DisableBracketedPaste)
        {
            errors.push(format!("bracketed paste: {error}"));
        }
        if self.keyboard_pushed
            && let Err(error) = stdout().execute(PopKeyboardEnhancementFlags)
        {
            errors.push(format!("keyboard flags: {error}"));
        }
        if self.mouse_captured
            && let Err(error) = stdout().execute(DisableMouseCapture)
        {
            errors.push(format!("mouse capture: {error}"));
        }
        if self.focus_change
            && let Err(error) = stdout().execute(DisableFocusChange)
        {
            errors.push(format!("focus change: {error}"));
        }
        // Mouse reporting is off above and raw mode is still on here: input
        // that reached the tty while the UI was not reading (a mouse moved
        // over a quitting screen) is discarded now, so it is never handed to
        // the shell as a burst of `\x1b[<…M` reports once cooked mode returns.
        discard_pending_terminal_input();
        if let Err(error) = ratatui::try_restore() {
            errors.push(format!("terminal restore: {error}"));
        }
        // Leaving the alternate screen does not restore DECTCEM on every
        // terminal; the last frame hid the cursor, so show it explicitly.
        if let Err(error) = stdout().execute(ShowCursor) {
            errors.push(format!("cursor: {error}"));
        }
        drop(held);
        errors
    }
}

/// Whether this process has the terminal in TUI modes right now.
static TERMINAL_HELD: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// How long a forced exit waits for an orderly restore already in progress.
const FORCED_RESTORE_LOCK_WAIT: Duration = Duration::from_millis(500);

/// The bytes that hand a TUI-held terminal back, in restoration order.
fn forced_restore_sequence() -> String {
    use crossterm::Command as _;
    let mut sequence = String::new();
    let _ = DisableMouseCapture.write_ansi(&mut sequence);
    let _ = DisableBracketedPaste.write_ansi(&mut sequence);
    let _ = PopKeyboardEnhancementFlags.write_ansi(&mut sequence);
    let _ = DisableFocusChange.write_ansi(&mut sequence);
    let _ = crossterm::terminal::LeaveAlternateScreen.write_ansi(&mut sequence);
    let _ = ShowCursor.write_ansi(&mut sequence);
    sequence
}

/// Restore the terminal from outside the UI thread, for a process about to
/// exit on a second terminating signal.
///
/// It runs on the signal watcher thread, never in a signal handler. Output
/// goes straight to the descriptor: the UI thread may hold the `Stdout` lock
/// while it paints, and a wedged paint must not keep the process alive. A
/// terminal the UI has already restored, or never took, is left alone.
pub(crate) fn force_restore_terminal() {
    let deadline = Instant::now() + FORCED_RESTORE_LOCK_WAIT;
    let mut held = loop {
        match TERMINAL_HELD.try_lock() {
            Ok(held) => break held,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(std::sync::TryLockError::WouldBlock) => return,
        }
    };
    if !std::mem::replace(&mut *held, false) {
        return;
    }
    let sequence = forced_restore_sequence();
    let mut bytes = sequence.as_bytes();
    while !bytes.is_empty() {
        match rustix::io::write(stdout(), bytes) {
            Ok(0) | Err(_) => break,
            Ok(count) => bytes = &bytes[count.min(bytes.len())..],
        }
    }
    let _ = crossterm::terminal::disable_raw_mode();
}

/// Bound on queued terminal events discarded at restore, so a pathological
/// input stream cannot keep the quit path reading forever.
pub(super) const RESTORE_INPUT_DISCARD_BUDGET: usize = 4096;

/// Drop input queued at the tty and in the event reader without acting on it.
pub(super) fn discard_pending_terminal_input() {
    let _ = std::io::Write::flush(&mut stdout());
    let _ = rustix::termios::tcflush(std::io::stdin(), rustix::termios::QueueSelector::IFlush);
    for _ in 0..RESTORE_INPUT_DISCARD_BUDGET {
        match event::poll(Duration::ZERO) {
            Ok(true) => {
                if event::read().is_err() {
                    return;
                }
            }
            Ok(false) | Err(_) => return,
        }
    }
}

impl Drop for TerminalRestoration {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub(super) fn install_terminal_panic_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut out = stdout();
            let _ = out.execute(DisableMouseCapture);
            let _ = out.execute(DisableBracketedPaste);
            let _ = out.execute(PopKeyboardEnhancementFlags);
            let _ = out.execute(DisableFocusChange);
            let _ = ratatui::try_restore();
            let _ = out.execute(ShowCursor);
            previous(info);
        }));
    });
}

pub(super) fn enable_terminal_modes(restoration: &mut TerminalRestoration, mouse_enabled: bool) {
    restoration.mouse_captured = mouse_enabled && stdout().execute(EnableMouseCapture).is_ok();
    restoration.bracketed_paste = stdout().execute(EnableBracketedPaste).is_ok();
    restoration.keyboard_pushed = stdout()
        .execute(PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
        ))
        .is_ok();
    restoration.focus_change = stdout().execute(EnableFocusChange).is_ok();
}

/// Owns one initialized terminal across TUI handoffs so resume stays in the
/// alternate screen instead of flashing the host shell.
pub struct PersistentTerminal {
    pub(super) terminal: DefaultTerminal,
    pub(super) restoration: TerminalRestoration,
    pub(super) mouse_enabled: bool,
}

impl PersistentTerminal {
    /// Enter the alternate screen and arm process-global restoration.
    ///
    /// # Errors
    ///
    /// Returns [`TuiError::NotATty`] when stdout is not a TTY, or [`TuiError::Io`]
    /// when terminal initialization fails.
    pub fn open(mouse_enabled: bool) -> Result<Self, TuiError> {
        if !stdout().is_tty() {
            return Err(TuiError::NotATty);
        }
        install_terminal_panic_hook();
        let terminal = ratatui::try_init().map_err(|error| from_io(&error))?;
        let mut restoration = TerminalRestoration::new();
        enable_terminal_modes(&mut restoration, mouse_enabled);
        *TERMINAL_HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        crate::lookup_status::tui_active(true);
        Ok(Self {
            terminal,
            restoration,
            mouse_enabled,
        })
    }

    /// Hand the terminal back now: mouse reporting, bracketed paste, keyboard
    /// flags and focus events off, queued input discarded, raw mode off, the
    /// alternate screen left and the cursor shown. Returns the cleanup errors.
    ///
    /// The launcher calls this the moment the UI reports [`TuiOutcome::Exited`],
    /// before it exits the process, so a slow session release is never awaited
    /// behind a frozen alternate screen with mouse reporting still on.
    #[must_use]
    pub fn close(mut self) -> Vec<String> {
        self.restoration.restore()
    }

    /// Drive one hosted UI session without leaving the alternate screen.
    ///
    /// # Errors
    ///
    /// Same as [`run_hosted`].
    #[allow(
        clippy::needless_pass_by_value,
        reason = "mirrors run_hosted: the session owns channel ends for the UI lifetime"
    )]
    pub fn run(
        &mut self,
        launch: TuiLaunch,
        actions: Option<crate::tui::UserActionSender>,
        events: Option<crate::tui::HostEventReceiver>,
    ) -> Result<TuiOutcome, TuiError> {
        if launch.mouse_enabled && !self.restoration.mouse_captured {
            self.restoration.mouse_captured = stdout().execute(EnableMouseCapture).is_ok();
        }
        self.mouse_enabled = launch.mouse_enabled;
        let hosted = actions.is_some();
        let mut app = App::new(launch, hosted);
        app.arm_launch_intro();
        run_app(
            &mut self.terminal,
            app,
            actions.as_ref(),
            events.as_ref(),
            self.mouse_enabled,
        )
    }
}

/// Owns host channel ends for the interactive session (dropped on exit).
#[allow(
    clippy::needless_pass_by_value,
    reason = "TUI owns mpsc ends for the session; public API transfers ownership"
)]
#[cfg(test)]
pub(crate) fn run_hosted(
    launch: TuiLaunch,
    actions: Option<crate::tui::UserActionSender>,
    events: Option<crate::tui::HostEventReceiver>,
) -> Result<TuiOutcome, TuiError> {
    let mut session = PersistentTerminal::open(launch.mouse_enabled)?;
    let result = session.run(launch, actions, events);
    let cleanup_errors = session.restoration.restore();
    combine_terminal_cleanup(result, &cleanup_errors)
}

pub(super) fn handle_terminal_event(
    app: &mut App,
    event: Event,
    actions: Option<&crate::tui::UserActionSender>,
    mouse_enabled: bool,
) {
    match event {
        Event::Key(key) => {
            if let Some(action) = app.handle_key(key) {
                dispatch_action(app, &action, actions);
            }
        }
        Event::Mouse(mouse) => {
            if let Some(action) = app.handle_mouse(mouse) {
                dispatch_action(app, &action, actions);
            }
        }
        Event::Paste(text) => {
            if let Some(action) = app.handle_paste(&text) {
                dispatch_action(app, &action, actions);
            }
        }
        Event::Resize(_, _) => {
            app.selection.clear();
            app.transcript_drag_pointer = None;
            app.reset_composer_drag();
            app.mark_dirty();
        }
        Event::FocusGained if mouse_enabled => {
            let _ = stdout().execute(EnableMouseCapture);
        }
        Event::FocusGained | Event::FocusLost => {}
    }
}

/// Drain at most `budget` pending terminal events.
///
/// Returns true when the budget ran out, so the loop can come straight back
/// instead of waiting: a paste or key-repeat burst is bounded per frame, never
/// per iteration. An exhausted budget is the only "more input" signal — asking
/// `poll` again would report a partially received escape sequence as ready and
/// send the loop into a blocking `read` for bytes that have not arrived.
pub(super) fn drain_terminal_input(
    app: &mut App,
    actions: Option<&crate::tui::UserActionSender>,
    mouse_enabled: bool,
    budget: usize,
) -> Result<bool, TuiError> {
    for _ in 0..budget {
        if !event::poll(Duration::ZERO).map_err(|error| from_io(&error))? {
            return Ok(false);
        }
        let event = event::read().map_err(|error| from_io(&error))?;
        handle_terminal_event(app, event, actions, mouse_enabled);
    }
    Ok(true)
}

/// One frame's worth of coalesced host observations.
#[derive(Clone, Copy, Default)]
pub(super) struct HostDrain {
    /// At least one observation was applied.
    pub(super) batched: bool,
    /// At least one assistant delta was appended to the tail block.
    pub(super) stream_delta: bool,
    /// More observations remain queued after the budget ran out.
    pub(super) pending: bool,
}

/// Apply up to `budget` host observations, coalescing them into one frame.
///
/// Deltas append to the tail block and the layout pass happens once, in the
/// single draw that follows (per-frame stream coalescing).
pub(super) fn drain_host_events(
    app: &mut App,
    events: &crate::tui::HostEventReceiver,
    host_connected: &mut bool,
    budget: usize,
) -> HostDrain {
    let mut drain = HostDrain::default();
    for _ in 0..budget {
        match events.try_recv() {
            Ok(event) => {
                drain.batched = true;
                drain.stream_delta |= matches!(event, HostEvent::AssistantDelta(_));
                app.apply_host_event(event);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return drain,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                *host_connected = false;
                if !app.should_quit {
                    app.note_host_disconnected();
                }
                return drain;
            }
        }
    }
    drain.pending = true;
    drain
}

/// Paint one frame inside a synchronized-update bracket when the terminal
/// honours DEC 2026, so a partially composed frame never reaches the screen.
pub(super) fn draw_frame(terminal: &mut DefaultTerminal, app: &mut App) -> Result<(), TuiError> {
    let synchronized = stdout().execute(BeginSynchronizedUpdate).is_ok();
    let result = terminal
        .draw(|frame| app.draw(frame))
        .map_err(|error| from_io(&error));
    if synchronized {
        let _ = stdout().execute(EndSynchronizedUpdate);
    }
    result.map(|_| crate::tui_timing::first_frame())
}

pub(super) fn run_app(
    terminal: &mut DefaultTerminal,
    mut app: App,
    actions: Option<&crate::tui::UserActionSender>,
    events: Option<&crate::tui::HostEventReceiver>,
    mouse_enabled: bool,
) -> Result<TuiOutcome, TuiError> {
    let schedule = RedrawSchedule::default();
    let mut host_connected = events.is_some();
    #[cfg(feature = "test-hooks")]
    let mut pilot =
        PilotSidecar::from_root(crate::test_hooks::tui_pilot_root()).map_err(TuiError::Io)?;
    loop {
        // Biased: terminal input is drained before host observations, so a
        // burst of stream deltas can never starve a keystroke or an interrupt.
        if let Some(actions) = actions {
            app.apply_process_snapshot(actions.processes());
        }
        let input_pending =
            drain_terminal_input(&mut app, actions, mouse_enabled, MAX_INPUT_DRAIN_PER_FRAME)?;
        // A `SIGTERM` or `SIGHUP` is a quit request: the same path `Ctrl+Q`
        // takes, so restoration and session release stay the ordinary ones.
        if crate::signals::signalled_stops() > 0 && !app.should_quit {
            dispatch_action(&mut app, &UserAction::Quit, actions);
        }
        let host = if host_connected && let Some(rx) = events {
            drain_host_events(&mut app, rx, &mut host_connected, MAX_HOST_EVENTS_PER_FRAME)
        } else {
            HostDrain::default()
        };

        app.maybe_request_tree_snapshot_after_attach(actions);
        app.maybe_refresh_composer_chrome(actions);
        app.maybe_request_agent_acknowledgements(actions);
        app.maybe_request_call_details(actions);
        app.sync_agent_transcript(actions);
        app.refresh_launch_binary_id();
        app.maybe_flush_pending_prompt_after_attach(actions);
        app.poll_attachments();
        if std::mem::take(&mut app.pending_reload) {
            // Asked while the session is still attached: once the handoff
            // starts there is nothing left to show a slow answer on.
            let workspace = std::path::PathBuf::from(&app.workspace);
            app.start_reload_probe(move || crate::carry::reload_target(&workspace));
        }
        if let Some(super::reload::ReloadOutcome::HandOff(target)) = app.poll_reload_probe() {
            request_terminal_handoff(&mut app, actions, TuiHandoff::Reload { target });
        }

        #[cfg(feature = "test-hooks")]
        if let Some(pilot) = pilot.as_mut()
            && let Some(action) = pilot.service(&mut app).map_err(TuiError::Io)?
        {
            dispatch_action(&mut app, &action, actions);
        }

        let wheel_drained = app.mouse_scroll_queue != 0;
        app.drain_mouse_scroll();
        app.settle_wheel_gesture();

        // Everything above is a state change; everything below is the clock.
        let state_dirty = app.dirty;
        let now = Instant::now();
        // A shown recent row dims or leaves at an absolute unix boundary.
        // The park poll still notices host observations; it must not repaint
        // the list on every tick. Running rows keep the spinner cadence.
        let now_unix = app.agents_now();
        let age_due = app.agents_list_open()
            && app
                .tree_navigator
                .as_ref()
                .is_some_and(|nav| nav.age_wake_at.is_some_and(|at| now_unix >= at));
        let animation_advanced = age_due;
        let spinner_advanced = app.advance_status_spinner(now);
        let drag_advanced =
            app.advance_transcript_drag_autoscroll() | app.advance_composer_drag_autoscroll();
        let mut timer_expired = app.recheck_installed_build(now);
        if let Some(arm) = &app.quit_arm
            && arm.expired(now)
        {
            app.quit_arm = None;
            timer_expired = true;
        }
        if let Some(arm) = &app.esc_clear_arm
            && arm.expired(now)
        {
            app.esc_clear_arm = None;
            timer_expired = true;
        }
        // The startup row ticks its own elapsed seconds while the host thread
        // is blocked opening the session and cannot publish progress.
        let session_open_ticking = app.session_open_started.is_some();

        let work = FrameWork {
            dirty: state_dirty || drag_advanced || timer_expired,
            host_batch: host.batched,
            stream_delta: host.stream_delta,
            spinner_advanced: spinner_advanced || session_open_ticking,
            animation_advanced,
            wheel_drained,
        };
        let reason = RedrawSchedule::reason(work);
        if reason.paints() || app.copy_selection_after_draw {
            draw_frame(terminal, &mut app)?;
            if let Some(actions) = actions {
                actions.mark_promptable();
            }
            if app.copy_selection_after_draw {
                if let Some(text) = app.take_transcript_selection_text() {
                    copy_text_osc52(&text);
                }
                app.copy_selection_after_draw = false;
            }
            app.dirty = false;
        }
        // The list frame arms the next boundary from its one projection. A
        // transcript overlay does not, so a due wake that is still due arms
        // the following boundary here instead of painting every poll.
        if age_due {
            let now_unix = app.agents_now();
            if let Some(nav) = app.tree_navigator.as_mut()
                && nav.age_wake_at.is_some_and(|at| now_unix >= at)
            {
                nav.set_now(now_unix);
                nav.refresh_age_wake();
            }
        }

        if app.should_quit {
            break;
        }

        // State-driven cadence: park when nothing animates, streams, or is
        // armed, and come straight back while a bounded drain left work.
        let wait = if input_pending || host.pending {
            Duration::ZERO
        } else {
            schedule
                .wait(app.redraw_demand())
                .bounded(redraw::PARK_POLL_CEILING)
        };
        let _ = event::poll(wait).map_err(|error| from_io(&error))?;
    }
    // `app.session_id` is the last `HostEvent::Binding` the host published, so
    // it is the only identity that survives an in-host session switch.
    let attached_session = app.session_id.clone();
    if let Some(handoff) = app.terminal_handoff {
        Ok(TuiOutcome::Handoff {
            handoff,
            attached_session,
        })
    } else if app.request_reattach {
        Ok(TuiOutcome::RetryAttach {
            attached_session,
            declined_resume_picker: app.declined_launch_resume_picker,
        })
    } else {
        Ok(TuiOutcome::Exited { attached_session })
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "central action dispatch keeps external side effects explicit"
)]
pub(super) fn dispatch_action(
    app: &mut App,
    action: &UserAction,
    actions: Option<&crate::tui::UserActionSender>,
) {
    match action {
        UserAction::Quit => {
            // Restoration comes first: the UI only ends itself here. The
            // launcher requests cancellation and shutdown on the mailbox once
            // the terminal is cooked, then joins the worker under the product
            // deadline.
            app.begin_quit();
        }
        UserAction::ResumeSession { id: Some(_) } => {
            if send_user_action(app, actions, action.clone()) {
                app.pending_open_tree = true;
            }
        }
        UserAction::ResumeSession { id: None } => {
            if send_user_action(app, actions, action.clone()) {
                app.pending_open_resume = true;
            }
        }
        UserAction::RetryAttach => {
            app.status_detail = Some(
                app.reattach_note
                    .take()
                    .unwrap_or_else(|| "switching model…".to_owned()),
            );
            if send_user_action(app, actions, UserAction::Quit) {
                app.request_reattach = true;
                app.should_quit = true;
            }
        }
        UserAction::Extension(SurfaceIntent::OpenConfigFile) => {
            request_terminal_handoff(app, actions, TuiHandoff::EditConfig);
        }
        UserAction::ResumeSessionHandoff { id } => {
            request_terminal_handoff(app, actions, TuiHandoff::ResumeSession { id: id.clone() });
        }
        UserAction::RefreshReadiness => {
            send_user_action(app, actions, action.clone());
        }
        UserAction::Extension(SurfaceIntent::OpenSettings) => {
            // The caller already chose the tab. `OpenSettings` only refreshes
            // the host snapshot; it must never re-route Ctrl+X or `/keybinds`
            // onto Settings (spec 02 §5.5).
            if app.control_tab.is_none() {
                app.control_tab = Some(ControlTab::Settings);
                app.pending_open_settings = true;
            }
            if app.settings_menu.is_none() {
                app.settings_menu = app.settings_snapshot.clone().map(SettingsMenu::open);
            }
            app.settings_error = None;
            app.slash_open = false;
            app.mark_dirty();
            // The launch snapshot renders immediately; this request refreshes
            // it from the product owner when startup preparation is ready.
            let _ = send_user_action(
                app,
                actions,
                UserAction::Extension(SurfaceIntent::OpenSettings),
            );
        }
        other => {
            if send_user_action(app, actions, other.clone()) {
                commit_sent_action(app, other);
            }
        }
    }
    app.mark_dirty();
}

pub(super) fn request_terminal_handoff(
    app: &mut App,
    actions: Option<&crate::tui::UserActionSender>,
    handoff: TuiHandoff,
) {
    if matches!(&handoff, TuiHandoff::Reload { .. })
        || send_user_action(app, actions, UserAction::Quit)
    {
        crate::tui_timing::quit_requested();
        app.terminal_handoff = Some(handoff);
        app.should_quit = true;
    }
}

pub(super) fn commit_sent_action(app: &mut App, action: &UserAction) {
    if action == &UserAction::ClearSession {
        app.clear_pending = true;
        "clear".clone_into(&mut app.phase);
        app.status_detail = Some("clear pending authoritative host acknowledgement".to_owned());
        return;
    }
    if !matches!(
        action,
        UserAction::Prompt(_) | UserAction::FollowUp(_) | UserAction::Steer(_)
    ) {
        return;
    }
    if let Some(echo) = local_echo_line(action, app.hosted) {
        app.push_block(ScrollBlock::user(echo));
    }
}

/// The single UI → host admission point. Refused text remains editable and
/// binding/config state is changed only by callers after this returns true.
pub(super) fn send_user_action(
    app: &mut App,
    actions: Option<&crate::tui::UserActionSender>,
    action: UserAction,
) -> bool {
    let Some(tx) = actions else {
        return true;
    };
    match tx.send(action.clone()) {
        Ok(()) => {
            match action {
                UserAction::Prompt(_) | UserAction::FollowUp(_) | UserAction::Steer(_) => {
                    app.pending_drafts.push_back(action);
                }
                UserAction::EditQueued { id, text, .. } => {
                    app.pending_queue_edit = Some((id, text));
                }
                _ => {}
            }
            true
        }
        Err(error) => {
            let reason = match &error {
                crate::tui::UserActionSendError::Saturated(_) => "host control queue is full",
                crate::tui::UserActionSendError::Disconnected(_) => "product host is disconnected",
                crate::tui::UserActionSendError::Duplicate(_) => {
                    "that session operation is already queued or running"
                }
            };
            match error.into_action() {
                UserAction::Prompt(draft)
                | UserAction::FollowUp(draft)
                | UserAction::Steer(draft) => {
                    let text = app.adopt_draft_images(draft);
                    app.set_input(text);
                    app.slash_open = false;
                    app.history_browse = None;
                }
                UserAction::EditQueued { text, .. } => {
                    app.set_input(text);
                    app.slash_open = false;
                    app.history_browse = None;
                }
                _ => {}
            }
            app.push_root_block(ScrollBlock::Error(format!(
                "{reason} — action not sent; retry when the host catches up"
            )));
            false
        }
    }
}

pub(super) fn display_workspace(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::forced_restore_sequence;

    /// The forced exit hands back every mode the UI takes, in the same order
    /// the ordinary restore uses: input modes off before the alternate screen
    /// is left, the cursor shown after it.
    #[test]
    fn the_forced_restore_releases_every_mode_before_leaving() {
        let sequence = forced_restore_sequence();
        let at = |needle: &str| {
            sequence
                .find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing from {sequence:?}"))
        };
        let leave = at("\x1b[?1049l");
        for mode in ["\x1b[?1000l", "\x1b[?2004l", "\x1b[<1u", "\x1b[?1004l"] {
            assert!(at(mode) < leave, "{mode:?} must precede the leave");
        }
        assert!(at("\x1b[?25h") > leave);
    }
}
