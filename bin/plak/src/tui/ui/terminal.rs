//! Terminal lifecycle, event loop, and action dispatch.

use super::cursor_backend::SteadyCursor;
use super::{
    App, BeginSynchronizedUpdate, ControlTab, DisableBracketedPaste, DisableFocusChange,
    DisableMouseCapture, Duration, EnableBracketedPaste, EnableFocusChange, EnableMouseCapture,
    EndSynchronizedUpdate, Event, FrameWork, HostEvent, Instant, KeyboardEnhancementFlags,
    MAX_HOST_EVENTS_PER_FRAME, MAX_INPUT_DRAIN_PER_FRAME, Path, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags, RedrawSchedule, ScrollBlock, SessionRequest, SettingsMenu,
    ShowCursor, Theme, TuiError, TuiLaunch, TuiOutcome, UserAction, event, local_echo_line, stdout,
};

/// The process terminal, with repeated cursor commands suppressed.
type PlakTerminal =
    ratatui::Terminal<SteadyCursor<ratatui::backend::CrosstermBackend<std::io::Stdout>>>;
use crate::tui::SurfaceIntent;
#[cfg(feature = "test-hooks")]
use crate::tui::ui::pilot_sidecar::PilotSidecar;
use crossterm::ExecutableCommand;
use crossterm::cursor::SetCursorStyle;
use crossterm::tty::IsTty;
use plakous_host_protocol::status::HostPhase;

// Keep printable input as terminal-composed text, not report-all key events.
const KEYBOARD_ENHANCEMENT_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

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

/// OSC 112, reset text cursor color. Sent only after this process set it.
const CURSOR_COLOR_RESET: &str = "\x1b]112\x07";

/// Packed `0x00RRGGBB`, or [`CURSOR_COLOR_UNSET`] when the terminal's own
/// cursor color is still in force. Atomic so a panic or forced exit on
/// another thread can reset it without taking a lock the painter holds.
const CURSOR_COLOR_UNSET: u32 = u32::MAX;
static CURSOR_COLOR: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(CURSOR_COLOR_UNSET);

/// Set when this process switched the caret to a steady block. Restore paths
/// send the terminal's own shape only if this is set.
static CURSOR_STYLE_STEADY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn pack_cursor_rgb(rgb: Option<(u8, u8, u8)>) -> u32 {
    match rgb {
        Some((red, green, blue)) => {
            (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)
        }
        None => CURSOR_COLOR_UNSET,
    }
}

fn unpack_cursor_rgb(packed: u32) -> Option<(u8, u8, u8)> {
    if packed == CURSOR_COLOR_UNSET {
        return None;
    }
    Some((
        u8::try_from((packed >> 16) & 0xff).unwrap_or(0),
        u8::try_from((packed >> 8) & 0xff).unwrap_or(0),
        u8::try_from(packed & 0xff).unwrap_or(0),
    ))
}

/// OSC 12 sets the cursor color; `None` is OSC 112.
pub(super) fn cursor_color_sequence(rgb: Option<(u8, u8, u8)>) -> String {
    match rgb {
        Some((red, green, blue)) => format!("\x1b]12;#{red:02x}{green:02x}{blue:02x}\x07"),
        None => CURSOR_COLOR_RESET.to_owned(),
    }
}

/// Light themes use the theme text color. Dark themes leave the cursor alone.
pub(super) fn light_theme_cursor_rgb(theme: &Theme) -> Option<(u8, u8, u8)> {
    theme.id.is_light().then(|| {
        let text = theme.colors.text;
        (text.red, text.green, text.blue)
    })
}

/// Bytes to emit when the desired cursor color differs from `current`.
pub(super) fn cursor_color_change(
    current: Option<(u8, u8, u8)>,
    desired: Option<(u8, u8, u8)>,
) -> Option<String> {
    (current != desired).then(|| cursor_color_sequence(desired))
}

fn emit_cursor_color(sequence: &str) {
    use std::io::Write as _;
    let mut terminal = stdout();
    let _ = terminal.write_all(sequence.as_bytes());
    let _ = terminal.flush();
}

/// Set or clear the cursor color when the active theme requires it.
///
/// A paint after release must not set the color: the restore path may already
/// have sent OSC 112. The held flag is taken for the whole emit so a forced
/// exit cannot reset the color and then lose the race to this write.
pub(super) fn sync_terminal_cursor_color(theme: &Theme) {
    let held = TERMINAL_HELD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !*held {
        return;
    }
    let desired = light_theme_cursor_rgb(theme);
    let current = unpack_cursor_rgb(CURSOR_COLOR.load(std::sync::atomic::Ordering::Relaxed));
    let Some(sequence) = cursor_color_change(current, desired) else {
        return;
    };
    CURSOR_COLOR.store(
        pack_cursor_rgb(desired),
        std::sync::atomic::Ordering::Relaxed,
    );
    emit_cursor_color(&sequence);
}

fn emit_cursor_style(style: SetCursorStyle) {
    let _ = stdout().execute(style);
}

/// Steady block for as long as this process holds the terminal.
///
/// A frame that drew cells has to move the caret back, and that move restarts
/// the blink timer. A steady shape does not show the restart.
fn set_steady_cursor_style() {
    emit_cursor_style(SetCursorStyle::SteadyBlock);
    CURSOR_STYLE_STEADY.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn reset_terminal_cursor_style() {
    if !take_cursor_style_override() {
        return;
    }
    emit_cursor_style(SetCursorStyle::DefaultUserShape);
}

fn take_cursor_style_override() -> bool {
    CURSOR_STYLE_STEADY.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// OSC 112 if this process set a cursor color. Dark themes never did.
fn reset_terminal_cursor_color() {
    if !take_cursor_color_override() {
        return;
    }
    emit_cursor_color(CURSOR_COLOR_RESET);
}

/// Take the override flag for a restore path that writes its own bytes.
fn take_cursor_color_override() -> bool {
    unpack_cursor_rgb(CURSOR_COLOR.swap(CURSOR_COLOR_UNSET, std::sync::atomic::Ordering::Relaxed))
        .is_some()
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
        reset_terminal_cursor_color();
        reset_terminal_cursor_style();
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
        super::terminal_panic::release();
        drop(held);
        errors
    }
}

/// Whether this process has the terminal in TUI modes right now.
static TERMINAL_HELD: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// How long a forced exit waits for an orderly restore already in progress.
const FORCED_RESTORE_LOCK_WAIT: Duration = Duration::from_millis(500);

/// The bytes that hand a TUI-held terminal back, in restoration order.
///
/// `reset_cursor_color` appends OSC 112 before the alternate screen is left,
/// and only when this process set the cursor color for a light theme.
/// `reset_cursor_style` appends the terminal's own cursor shape on that same
/// side of the leave, and only when this process set the steady block.
fn forced_restore_sequence(reset_cursor_color: bool, reset_cursor_style: bool) -> String {
    use crossterm::Command as _;
    let mut sequence = String::new();
    let _ = DisableMouseCapture.write_ansi(&mut sequence);
    let _ = DisableBracketedPaste.write_ansi(&mut sequence);
    let _ = PopKeyboardEnhancementFlags.write_ansi(&mut sequence);
    let _ = DisableFocusChange.write_ansi(&mut sequence);
    if reset_cursor_color {
        sequence.push_str(CURSOR_COLOR_RESET);
    }
    if reset_cursor_style {
        let _ = SetCursorStyle::DefaultUserShape.write_ansi(&mut sequence);
    }
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
    let sequence =
        forced_restore_sequence(take_cursor_color_override(), take_cursor_style_override());
    let mut bytes = sequence.as_bytes();
    while !bytes.is_empty() {
        match rustix::io::write(stdout(), bytes) {
            Ok(0) | Err(_) => break,
            Ok(count) => bytes = &bytes[count.min(bytes.len())..],
        }
    }
    let _ = crossterm::terminal::disable_raw_mode();
    super::terminal_panic::release();
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
    super::terminal_panic::install(|| {
        let mut out = stdout();
        let _ = out.execute(DisableMouseCapture);
        let _ = out.execute(DisableBracketedPaste);
        let _ = out.execute(PopKeyboardEnhancementFlags);
        let _ = out.execute(DisableFocusChange);
        reset_terminal_cursor_color();
        reset_terminal_cursor_style();
        let _ = ratatui::try_restore();
        let _ = out.execute(ShowCursor);
    });
}

pub(super) fn enable_terminal_modes(restoration: &mut TerminalRestoration, mouse_enabled: bool) {
    restoration.mouse_captured = mouse_enabled && stdout().execute(EnableMouseCapture).is_ok();
    restoration.bracketed_paste = stdout().execute(EnableBracketedPaste).is_ok();
    restoration.keyboard_pushed = stdout()
        .execute(PushKeyboardEnhancementFlags(KEYBOARD_ENHANCEMENT_FLAGS))
        .is_ok();
    restoration.focus_change = stdout().execute(EnableFocusChange).is_ok();
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InitStep {
    RawMode,
    AlternateScreen,
}

fn initialize_terminal(
    #[cfg(test)] mut after_step: impl FnMut(InitStep) -> Result<(), TuiError>,
) -> Result<(PlakTerminal, TerminalRestoration), TuiError> {
    let restoration = TerminalRestoration::new();
    super::terminal_panic::claim();
    // The library's init helper installs an unconditional panic hook.
    crossterm::terminal::enable_raw_mode().map_err(|error| from_io(&error))?;
    #[cfg(test)]
    after_step(InitStep::RawMode)?;
    stdout()
        .execute(crossterm::terminal::EnterAlternateScreen)
        .map_err(|error| from_io(&error))?;
    #[cfg(test)]
    after_step(InitStep::AlternateScreen)?;
    let terminal = ratatui::Terminal::new(SteadyCursor::new(
        ratatui::backend::CrosstermBackend::new(stdout()),
    ))
    .map_err(|error| from_io(&error))?;
    Ok((terminal, restoration))
}

/// Owns one initialized terminal across TUI handoffs so resume stays in the
/// alternate screen instead of flashing the host shell.
pub struct PersistentTerminal {
    pub(super) terminal: PlakTerminal,
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
        let (terminal, mut restoration) = initialize_terminal(
            #[cfg(test)]
            |_| Ok(()),
        )?;
        enable_terminal_modes(&mut restoration, mouse_enabled);
        // Held across the style write, so a forced exit either waits and then
        // restores the shape, or runs before the terminal is marked held.
        let mut held = TERMINAL_HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *held = true;
        set_steady_cursor_style();
        drop(held);
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
        actions: Option<crate::tui::FaceActionSender>,
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
    actions: Option<crate::tui::FaceActionSender>,
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
    actions: Option<&crate::tui::FaceActionSender>,
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
            app.pane.selection.clear();
            app.transcript_drag_pointer = None;
            app.reset_composer_drag();
            app.mark_dirty();
        }
        Event::FocusGained => {
            app.composer_chrome.refresh_pending = true;
            if mouse_enabled {
                let _ = stdout().execute(EnableMouseCapture);
            }
        }
        Event::FocusLost => {}
    }
}

/// Drain at most `budget` pending terminal events and return how many.
///
/// A count equal to the budget means the budget ran out, so the loop can come
/// straight back instead of waiting: a paste or key-repeat burst is bounded per frame, never
/// per iteration. An exhausted budget is the only "more input" signal — asking
/// `poll` again would report a partially received escape sequence as ready and
/// send the loop into a blocking `read` for bytes that have not arrived.
pub(super) fn drain_terminal_input(
    app: &mut App,
    actions: Option<&crate::tui::FaceActionSender>,
    mouse_enabled: bool,
    budget: usize,
) -> Result<usize, TuiError> {
    for taken in 0..budget {
        if !event::poll(Duration::ZERO).map_err(|error| from_io(&error))? {
            return Ok(taken);
        }
        let event = event::read().map_err(|error| from_io(&error))?;
        handle_terminal_event(app, event, actions, mouse_enabled);
    }
    Ok(budget)
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
    app.note_transcript_gaps(events);
    let mut drain = HostDrain::default();
    while let Some(unsent) = events.try_unsent() {
        use super::unsent::Unsent;
        use plakous_host_protocol::client::SendError;
        drain.batched = true;
        app.unsent_action(match unsent {
            SendError::Refused(error) => Unsent::Refused(error),
            SendError::RefusedForRole { row, intent } => Unsent::ForRole {
                row,
                action: intent,
            },
            SendError::OutcomeUnknown { intent, .. } => Unsent::Unknown(intent),
        });
    }
    app.rearm_due(app.now());
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
                    if let Some(reason) = events.ended() {
                        app.push_root_block(ScrollBlock::Error(format!(
                            "host transport ended: {reason}"
                        )));
                    }
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
pub(super) fn draw_frame(terminal: &mut PlakTerminal, app: &mut App) -> Result<(), TuiError> {
    sync_terminal_cursor_color(&app.theme);
    let synchronized = stdout().execute(BeginSynchronizedUpdate).is_ok();
    let result = terminal
        .draw(|frame| app.draw(frame))
        .map_err(|error| from_io(&error));
    if synchronized {
        let _ = stdout().execute(EndSynchronizedUpdate);
    }
    result.map(|_| crate::tui_timing::first_frame())
}

#[allow(
    clippy::too_many_lines,
    reason = "The terminal event loop orders host drains, input, redraw, resize, and quit settlement around one terminal owner."
)]
pub(super) fn run_app(
    terminal: &mut PlakTerminal,
    mut app: App,
    actions: Option<&crate::tui::FaceActionSender>,
    events: Option<&crate::tui::HostEventReceiver>,
    mouse_enabled: bool,
) -> Result<TuiOutcome, TuiError> {
    let schedule = RedrawSchedule::default();
    let mut host_connected = events.is_some();
    crate::tui::wake::arm().map_err(|error| from_io(&error))?;
    if let Some(events) = events {
        events.set_notify(crate::tui::wake::wake);
    }
    #[cfg(feature = "test-hooks")]
    let mut pilot =
        PilotSidecar::from_root(crate::test_hooks::tui::pilot_root()).map_err(TuiError::Io)?;
    let mut input_watch = crate::tui::wake::InputWatch::default();
    let mut input_seen = crate::tui::wake::Input::Quiet;
    let mut promptable_sent = false;
    loop {
        // Biased: terminal input is drained before host observations, so a
        // burst of stream deltas can never starve a keystroke or an interrupt.
        // A hung-up terminal is a quit, as `SIGHUP` is: crossterm's reader
        // would spin on the dead descriptor, so it is not drained.
        let input_events = if input_seen == crate::tui::wake::Input::HungUp {
            if !app.should_quit {
                dispatch_action(&mut app, &UserAction::Quit, actions);
            }
            0
        } else {
            drain_terminal_input(&mut app, actions, mouse_enabled, MAX_INPUT_DRAIN_PER_FRAME)?
        };
        let input_pending = input_events == MAX_INPUT_DRAIN_PER_FRAME;
        input_watch.observe(input_seen, input_events);
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
        app.maybe_reread_usage(actions);
        app.maybe_sync_usage_watch(actions);
        app.maybe_request_agent_acknowledgements(actions);
        app.maybe_request_call_details(actions);
        app.sync_agent_transcript(actions);
        app.refresh_launch_binary_id();
        app.maybe_flush_pending_prompt_after_attach(actions);
        app.poll_attachments();
        app.poll_usage_decoration();
        let host_notice = events.and_then(crate::tui::HostEventReceiver::host_notice);
        if app.host_notice != host_notice {
            app.host_notice = host_notice;
            app.mark_dirty();
        }
        if let Some(cause) = app.pending_rebind.take() {
            request_terminal_handoff(&mut app, actions, SessionRequest::Rebind { cause });
        }
        app.service_reload_request();
        if std::mem::take(&mut app.pending_stop_session)
            && let Some(session) = app.session_id.as_deref()
            && !actions.is_some_and(|actions| actions.stop_session(session))
        {
            app.push_block(ScrollBlock::System(
                "stop refused: the host is disconnected".to_owned(),
            ));
        }
        if let Some(action) = app.new_session_after_stop(Instant::now()) {
            dispatch_action(&mut app, &action, actions);
        }
        if std::mem::take(&mut app.pending_reload) {
            // Asked while the session is still attached: once the handoff
            // starts there is nothing left to show a slow answer on.
            app.start_host_reload_probe(actions);
        }
        match app.poll_reload_probe() {
            Some(super::reload::ReloadOutcome::HandOff(target)) => {
                request_terminal_handoff(
                    &mut app,
                    actions,
                    SessionRequest::ReloadRelease { target },
                );
            }
            Some(super::reload::ReloadOutcome::Refused) | None => {}
        }
        app.send_reload_answers(actions);

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
        // A shown recent row dims or leaves at an absolute unix boundary, a
        // one-shot deadline of its own; it must not repaint the list on every
        // wake. Running rows keep the spinner cadence.
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
        app.recheck_installed_build(now, actions);
        let mut timer_expired = false;
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
            #[cfg(test)]
            if let Some(after_draw) = app.after_draw.take() {
                after_draw();
            }
            // Once, after the first painted frame: the host learns the face
            // takes input. Not queued work.
            if !promptable_sent && let Some(actions) = actions {
                promptable_sent = actions.send(UserAction::Promptable).is_ok();
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
        // the following boundary here instead of painting every wake.
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
            Some(Duration::ZERO)
        } else {
            schedule
                .wait(app.redraw_demand())
                .bound(app.next_deadline(Instant::now()))
        };
        let (wait, input_mode) = input_watch.next_wait(wait);
        input_seen = crate::tui::wake::wait(wait, input_mode).map_err(|error| from_io(&error))?;
    }
    // `app.session_id` is the last `HostEvent::Binding` the host published, so
    // it is the only identity that survives an in-host session switch.
    let attached_session = app.session_id.clone();
    if let Some(request) = app.terminal_handoff {
        Ok(TuiOutcome::Request {
            request,
            attached_session,
        })
    } else if app.request_reattach {
        Ok(TuiOutcome::Request {
            request: SessionRequest::RetryAttach {
                declined_resume_picker: app.declined_launch_resume_picker,
            },
            attached_session,
        })
    } else {
        Ok(TuiOutcome::Exited { attached_session })
    }
}

pub(super) fn dispatch_action(
    app: &mut App,
    action: &UserAction,
    actions: Option<&crate::tui::FaceActionSender>,
) {
    app.drop_new_after_stop_for(action);
    match action {
        UserAction::Quit => {
            // Restoration comes first: the UI only ends itself here. The
            // launcher requests cancellation and shutdown on the mailbox once
            // the terminal is cooked, then joins the worker under the product
            // deadline.
            app.begin_quit();
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
            if app.host_mode || send_user_action(app, actions, UserAction::Quit) {
                app.request_reattach = true;
                app.should_quit = true;
            }
        }
        UserAction::Extension(SurfaceIntent::OpenConfigFile) => {
            request_terminal_handoff(app, actions, SessionRequest::EditConfig);
        }
        UserAction::ResumeSessionHandoff { id } => {
            request_terminal_handoff(
                app,
                actions,
                SessionRequest::ResumeSession {
                    id: id.clone(),
                    held_reload_requests: app.held_reload_requests(),
                },
            );
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
    actions: Option<&crate::tui::FaceActionSender>,
    handoff: SessionRequest,
) {
    if matches!(
        &handoff,
        SessionRequest::ReloadRelease { .. } | SessionRequest::Rebind { .. }
    ) || app.host_mode
        || send_user_action(app, actions, UserAction::Quit)
    {
        crate::tui_timing::quit_requested();
        app.terminal_handoff = Some(handoff);
        app.should_quit = true;
    }
}

pub(super) fn commit_sent_action(app: &mut App, action: &UserAction) {
    if action == &UserAction::NewSession {
        app.clear_pending = true;
        app.phase = HostPhase::Clear;
        app.status_detail =
            Some("new session pending authoritative host acknowledgement".to_owned());
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
    actions: Option<&crate::tui::FaceActionSender>,
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
            let reason = refusal_reason(&error);
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

pub(super) fn refusal_reason(error: &crate::tui::UserActionSendError) -> String {
    match error {
        crate::tui::UserActionSendError::Saturated(_) => "host control queue is full",
        crate::tui::UserActionSendError::Disconnected(_) => "product host is disconnected",
        crate::tui::UserActionSendError::Duplicate(_) => {
            "that session operation is already queued or running"
        }
        crate::tui::UserActionSendError::Refused(_) => "the session refused it",
    }
    .to_owned()
}

/// The text of a refusal for `row` of the role table.
pub(super) fn role_refusal_reason(row: &str) -> String {
    match row {
        "transcript" => "another attachment holds the session's agent view".to_owned(),
        row => format!("this face may not send that ({row})"),
    }
}

pub(super) fn display_workspace(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::forced_restore_sequence;

    #[test]
    fn keyboard_mode_preserves_terminal_text_and_reports_alternate_keys() {
        use crossterm::Command as _;
        let flags = super::KEYBOARD_ENHANCEMENT_FLAGS;
        assert_eq!(flags.bits(), 1 | 2 | 4);
        assert!(!flags.contains(super::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES));
        let mut sequence = String::new();
        super::PushKeyboardEnhancementFlags(flags)
            .write_ansi(&mut sequence)
            .unwrap();
        assert_eq!(sequence, "\x1b[>7u");
    }

    #[test]
    fn terminal_restoration_guard_runs_on_normal_exit_and_error_return() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let restored = Arc::new(AtomicUsize::new(0));
        let exit = |fail| -> Result<(), &'static str> {
            let observed = Arc::clone(&restored);
            let _guard = super::TerminalRestoration::with_test_hook(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            });
            if fail {
                return Err("injected terminal error return");
            }
            Ok(())
        };
        assert_eq!(exit(false), Ok(()));
        assert_eq!(restored.load(Ordering::SeqCst), 1);
        assert!(exit(true).is_err());
        assert_eq!(restored.load(Ordering::SeqCst), 2);
    }

    /// The forced exit hands back every mode the UI takes, in the same order
    /// the ordinary restore uses: input modes off before the alternate screen
    /// is left, the cursor shown after it.
    #[test]
    fn the_forced_restore_releases_every_mode_before_leaving() {
        let sequence = forced_restore_sequence(false, false);
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
        assert!(
            !sequence.contains("\x1b]112"),
            "a dark session must not reset a cursor color it never set"
        );
    }

    #[test]
    fn a_light_theme_cursor_reset_precedes_leaving_the_alternate_screen() {
        let theme = super::Theme::for_id(crate::tui::theme::ThemeId::DefaultLight);
        let rgb = super::light_theme_cursor_rgb(&theme).expect("light themes set a color");
        assert_eq!(
            super::cursor_color_sequence(Some(rgb)),
            "\x1b]12;#181a1f\x07"
        );
        assert!(
            super::light_theme_cursor_rgb(&super::Theme::for_id(
                crate::tui::theme::ThemeId::DefaultDark
            ))
            .is_none()
        );
        assert_eq!(
            super::cursor_color_change(None, Some(rgb)).as_deref(),
            Some("\x1b]12;#181a1f\x07")
        );
        assert!(super::cursor_color_change(Some(rgb), Some(rgb)).is_none());
        assert_eq!(
            super::cursor_color_change(Some(rgb), None).as_deref(),
            Some("\x1b]112\x07")
        );

        let sequence = forced_restore_sequence(true, false);
        let at = |needle: &str| {
            sequence
                .find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing from {sequence:?}"))
        };
        let leave = at("\x1b[?1049l");
        assert!(at("\x1b]112\x07") < leave);
        assert!(
            !sequence.contains("\x1b[0 q"),
            "a color reset does not imply a cursor-shape reset"
        );
    }

    #[test]
    fn a_steady_cursor_shape_is_restored_before_leaving_the_alternate_screen() {
        let sequence = forced_restore_sequence(false, true);
        let at = |needle: &str| {
            sequence
                .find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing from {sequence:?}"))
        };
        assert!(at("\x1b[0 q") < at("\x1b[?1049l"));
        assert!(!forced_restore_sequence(false, false).contains("\x1b[0 q"));
    }

    #[test]
    fn a_paint_after_release_does_not_set_the_cursor_color() {
        use std::sync::atomic::Ordering;
        let theme = super::Theme::for_id(crate::tui::theme::ThemeId::DefaultLight);
        let was_held = {
            let mut held = super::TERMINAL_HELD
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let was_held = *held;
            *held = false;
            was_held
        };
        let before = super::CURSOR_COLOR.load(Ordering::Relaxed);
        super::sync_terminal_cursor_color(&theme);
        assert_eq!(
            super::CURSOR_COLOR.load(Ordering::Relaxed),
            before,
            "a released terminal must not take a cursor color"
        );
        *super::TERMINAL_HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = was_held;
    }
}
