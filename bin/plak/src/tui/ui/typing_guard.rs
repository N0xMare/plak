//! How long since the operator last typed in the composer (spec 02 §5.2).
//! An empty `Enter` sends a queued prompt only after [`SEND_NOW_GAP`], and
//! `?` on an empty draft opens Keybinds only after [`QUIET`]. Views with no
//! prompt do not consult this.

use super::{Instant, KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;

/// A key within this long of the last typing key is still typing.
pub(super) const QUIET: Duration = Duration::from_millis(300);
/// No typing key, `Enter`, `Tab` or paste for this long: a plain `Enter` may
/// send the oldest queued prompt now.
pub(super) const SEND_NOW_GAP: Duration = Duration::from_secs(2);
/// A painted chip fires on a click only once it has held its place this long.
pub(super) const CHIP_SETTLE: Duration = Duration::from_millis(300);

/// The plain keys that type text: printable characters, Enter and Backspace.
/// Ctrl and Alt chords are commands, not typing.
fn is_typing_key(key: &KeyEvent) -> bool {
    !is_chord(key)
        && matches!(
            key.code,
            KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace
        )
}

/// A `Ctrl`, `Alt` or `Super` chord.
fn is_chord(key: &KeyEvent) -> bool {
    key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

/// What was known before the current key arrived.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Typing {
    /// No typing key (or `Tab`) arrived within [`QUIET`].
    pub(super) quiet: bool,
    /// How long ago the last typing key, `Tab` or paste arrived.
    pub(super) since_typed: Duration,
}

impl Typing {
    /// A key with no typed run before it: a click on a painted key label,
    /// or the pilot's semantic action.
    pub(super) const SETTLED: Self = Self {
        quiet: true,
        since_typed: Duration::MAX,
    };

    /// Not inside a typed run: no typing key within [`QUIET`], so `?` on an
    /// empty draft may open Keybinds.
    pub(super) const fn settled(self) -> bool {
        self.quiet
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct TypingGuard {
    /// When the last typing key, `Tab` or paste arrived.
    last_typed: Option<Instant>,
}

impl TypingGuard {
    /// Records `key` at `now` and returns what was known before it.
    pub(super) fn observe(&mut self, key: &KeyEvent, now: Instant) -> Typing {
        let since = self
            .last_typed
            .map_or(Duration::MAX, |at| now.saturating_duration_since(at));
        let before = Typing {
            quiet: since >= QUIET,
            since_typed: since,
        };
        if is_typing_key(key)
            || (!is_chord(key) && matches!(key.code, KeyCode::Tab | KeyCode::BackTab))
        {
            self.last_typed = Some(now);
        }
        before
    }

    /// A paste is typed text, as far as what follows it is concerned.
    pub(super) fn observe_paste(&mut self, now: Instant) {
        self.last_typed = Some(now);
    }

    /// Forgets the typed run, for keys that are not the operator's: the pilot's
    /// semantic actions and painted key labels.
    pub(super) fn forget_run(&mut self) {
        self.last_typed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{QUIET, SEND_NOW_GAP, TypingGuard};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::Instant;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn quiet_waits_out_typing_keys_and_an_arrow_is_not_one() {
        let start = Instant::now();
        let mut guard = TypingGuard::default();
        guard.observe(&press(KeyCode::Char('s')), start);
        let later = start + QUIET;
        let before = guard.observe(&press(KeyCode::Right), later);
        assert!(before.settled());
        assert_eq!(before.since_typed, QUIET);
        // An arrow is not typing, so it does not restart the quiet window.
        let before = guard.observe(&press(KeyCode::Char('?')), later + QUIET / 2);
        assert!(before.settled(), "an arrow is not typing");
        let before = guard.observe(&press(KeyCode::Char('x')), later + QUIET);
        assert!(
            !before.settled(),
            "a letter inside the quiet window is still typing"
        );
    }

    #[test]
    fn a_paste_counts_as_typing_for_the_send_now_gap() {
        let start = Instant::now();
        let mut guard = TypingGuard::default();
        guard.observe_paste(start);
        // A non-typing key reads the gap without restarting it. Enter is itself
        // a typing key, so a second Enter would measure from the first.
        let before = guard.observe(&press(KeyCode::Right), start + SEND_NOW_GAP / 2);
        assert!(before.since_typed < SEND_NOW_GAP);
        let before = guard.observe(&press(KeyCode::Right), start + SEND_NOW_GAP);
        assert!(before.since_typed >= SEND_NOW_GAP);
        let mut guard = TypingGuard::default();
        guard.observe_paste(start);
        let before = guard.observe(&press(KeyCode::Enter), start + SEND_NOW_GAP);
        assert!(before.since_typed >= SEND_NOW_GAP);
    }
}
