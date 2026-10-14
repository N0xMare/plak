//! The keymap registry (spec 02 §5.2).
//!
//! One table of bindings, grouped by the `When` context the input layer is in.
//! The Keybinds tab is generated from this table, so a binding cannot exist in
//! the input layer without appearing on the sheet, and the sheet cannot invent
//! one. Double-tap windows come from `crate::tui::quit_guard`, which owns them.

use std::time::Duration;

use crate::tui::quit_guard::{ESC_CLEAR_WINDOW, QUIT_CONFIRM_WINDOW};

/// Input context a binding is live in.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum When {
    /// Live wherever the shell has focus.
    Global,
    /// Live while the composer holds focus.
    Composer,
    /// Live over the transcript and the queue.
    Transcript,
    /// Live while an agent's transcript is the Activity > Agents body.
    Agent,
    /// Live inside the Menu modal.
    Modal,
    /// Live inside the Activity Agents list.
    Tree,
    /// Live inside Activity's Processes tab.
    Processes,
    /// Live inside the resume picker.
    Resume,
    /// Live inside a floating text overlay such as `/help`.
    Overlay,
    /// Live inside the confirmation pop-up.
    Confirm,
}

impl When {
    /// Contexts in sheet order.
    pub const ALL: [Self; 10] = [
        Self::Global,
        Self::Composer,
        Self::Transcript,
        Self::Agent,
        Self::Modal,
        Self::Tree,
        Self::Processes,
        Self::Resume,
        Self::Overlay,
        Self::Confirm,
    ];

    /// Sheet heading for this context.
    #[must_use]
    pub const fn heading(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Composer => "composer",
            Self::Transcript => "transcript and queue",
            Self::Agent => "agent transcript",
            Self::Modal => "Menu",
            Self::Tree => "Agents list",
            Self::Processes => "Activity processes",
            Self::Resume => "resume picker",
            Self::Overlay => "help overlay",
            Self::Confirm => "confirmation",
        }
    }
}

/// A binding that only fires on a repeated tap inside a window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DoubleTap {
    /// Taps required.
    pub taps: u8,
    /// Window the taps must fall inside.
    pub window: Duration,
}

/// One registered binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Binding {
    /// Chord text, exactly as the sheet prints it.
    pub chord: &'static str,
    /// Context the chord is live in.
    pub when: When,
    /// What the chord does, phrased as an effect.
    pub effect: &'static str,
    /// Repeat requirement, when the chord is a double-tap.
    pub double_tap: Option<DoubleTap>,
}

const fn tap(taps: u8, window: Duration) -> DoubleTap {
    DoubleTap { taps, window }
}

/// Every binding the input layer implements today.
pub const BINDINGS: &[Binding] = &[
    // Global.
    Binding {
        chord: "Cmd/Super+C",
        when: When::Global,
        effect: "copy the current selection; never cancel or quit",
        double_tap: None,
    },
    Binding {
        chord: "Cmd/Super+X",
        when: When::Composer,
        effect: "cut the composer selection",
        double_tap: None,
    },
    Binding {
        chord: "Cmd/Super+A",
        when: When::Composer,
        effect: "select the whole draft",
        double_tap: None,
    },
    Binding {
        chord: "Cmd/Super+Z",
        when: When::Composer,
        effect: "undo the last composer edit",
        double_tap: None,
    },
    Binding {
        chord: "Cmd/Super+Shift+Z",
        when: When::Composer,
        effect: "redo the last composer edit",
        double_tap: None,
    },
    Binding {
        chord: "Esc",
        when: When::Global,
        effect: "close the focused owner; ask to stop this turn while busy, in the confirmation pop-up",
        double_tap: None,
    },
    Binding {
        chord: "?",
        when: When::Composer,
        effect: "open Keybinds with an empty draft after a pause in typing; otherwise type ?",
        double_tap: None,
    },
    Binding {
        chord: "Esc Esc",
        when: When::Global,
        effect: "clear the draft while idle — undo with Ctrl+Z",
        double_tap: Some(tap(2, ESC_CLEAR_WINDOW)),
    },
    Binding {
        chord: "Ctrl+C",
        when: When::Global,
        effect: "copy a selection; otherwise clear the draft, ask to stop the turn in the confirmation pop-up, quit a stuck cancel, or quit while idle — second press within 1000 ms",
        double_tap: Some(tap(2, QUIT_CONFIRM_WINDOW)),
    },
    Binding {
        chord: "Ctrl+Q",
        when: When::Global,
        effect: "quit the TUI; asks first while a turn, an agent or a background process runs",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+X",
        when: When::Global,
        effect: "cut the composer selection; with no selection reopen Menu on the last-used tab",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+T",
        when: When::Global,
        effect: "open Activity on the last tab, or close it when Activity is open",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+O",
        when: When::Global,
        effect: "open or close the focused transcript row",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+D",
        when: When::Global,
        effect: "drop the selected queued follow-up when the queue is focused; otherwise delete the character after the caret",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+Shift+O",
        when: When::Global,
        effect: "fold everything open, else open every group (needs kitty keyboard flags)",
        double_tap: None,
    },
    Binding {
        chord: "Alt+O",
        when: When::Global,
        effect: "same, on a terminal without the kitty keyboard protocol",
        double_tap: None,
    },
    Binding {
        chord: "/",
        when: When::Global,
        effect: "open the slash command palette from an empty draft",
        double_tap: None,
    },
    // Composer.
    Binding {
        chord: "Ctrl+B",
        when: When::Global,
        effect: "background in-flight foreground commands; no-op otherwise",
        double_tap: None,
    },
    Binding {
        chord: "Enter",
        when: When::Composer,
        effect: "send while idle; queue a follow-up while busy",
        double_tap: None,
    },
    Binding {
        chord: "Shift+Enter / Alt+Enter",
        when: When::Composer,
        effect: "insert a newline in the draft",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+Enter / Ctrl+S",
        when: When::Composer,
        effect: "steer while busy; send now while idle",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+A / Ctrl+E",
        when: When::Composer,
        effect: "move the caret to line start or line end",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+Shift+A",
        when: When::Composer,
        effect: "select the whole draft (needs kitty keyboard flags)",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+U / Ctrl+K",
        when: When::Composer,
        effect: "kill to line start or to line end",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+W / Alt+Backspace",
        when: When::Composer,
        effect: "delete the word before the caret",
        double_tap: None,
    },
    Binding {
        chord: "Alt+D",
        when: When::Composer,
        effect: "delete the word after the caret (with an effect decision open, ask to deny it)",
        double_tap: None,
    },
    Binding {
        chord: "Alt+B / Alt+F",
        when: When::Composer,
        effect: "move the caret one word left or right",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+← / Ctrl+→",
        when: When::Composer,
        effect: "move the caret one word left or right",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+Z / Ctrl+Shift+Z",
        when: When::Composer,
        effect: "undo or redo the draft — Ctrl+Y also redoes",
        double_tap: None,
    },
    Binding {
        chord: "↑ (empty draft)",
        when: When::Composer,
        effect: "recall the prior prompt from history",
        double_tap: None,
    },
    Binding {
        chord: "Tab (palette open)",
        when: When::Composer,
        effect: "complete the highlighted slash command into the draft when the typed word begins its name",
        double_tap: None,
    },
    // Transcript and queue.
    Binding {
        chord: "wheel",
        when: When::Transcript,
        effect: "scroll the transcript",
        double_tap: None,
    },
    Binding {
        chord: "click scrollbar track",
        when: When::Transcript,
        effect: "page the transcript",
        double_tap: None,
    },
    Binding {
        chord: "drag scrollbar thumb",
        when: When::Transcript,
        effect: "move to a document position without selecting text",
        double_tap: None,
    },
    Binding {
        chord: "click a tool row",
        when: When::Transcript,
        effect: "expand or collapse that tool's detail",
        double_tap: None,
    },
    Binding {
        chord: "click [now] / [drop]",
        when: When::Transcript,
        effect: "send a queued follow-up now, or drop it, once the chip has held its place 300 ms",
        double_tap: None,
    },
    Binding {
        chord: "Tab",
        when: When::Transcript,
        effect: "focus the queue when it is not empty",
        double_tap: None,
    },
    Binding {
        chord: "↑ / ↓",
        when: When::Transcript,
        effect: "select a queued follow-up",
        double_tap: None,
    },
    Binding {
        chord: "Shift+↑ / Shift+↓",
        when: When::Transcript,
        effect: "reorder the selected follow-up",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+E",
        when: When::Transcript,
        effect: "edit the selected follow-up in the draft (queue focused); any other key leaves the queue for the composer",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+S",
        when: When::Transcript,
        effect: "send the selected follow-up now (queue focused, whatever the draft holds)",
        double_tap: None,
    },
    Binding {
        chord: "Ctrl+R",
        when: When::Transcript,
        effect: "resume a restored queue drain (queue focused)",
        double_tap: None,
    },
    Binding {
        chord: "Enter",
        when: When::Transcript,
        effect: "cycle the focused tool row; on the focused queue, send the selected follow-up now (a paused drain too), or only leave when delivery is unavailable",
        double_tap: None,
    },
    Binding {
        chord: "Alt+A / Alt+D",
        when: When::Transcript,
        effect: "ask to allow or deny the open effect decision in the confirmation pop-up (Option as Meta on macOS)",
        double_tap: None,
    },
    Binding {
        chord: "click [allow] / [deny]",
        when: When::Transcript,
        effect: "allow or deny the open effect decision",
        double_tap: None,
    },
    // Menu. Ctrl+X is a Global chord and is listed there once; it
    // never reaches `handle_control_key`, so it has no row here.
    // Agent transcript.
    Binding {
        chord: "Esc",
        when: When::Agent,
        effect: "back to the Agents list at this agent; never stops the root turn",
        double_tap: None,
    },
    Binding {
        chord: "↑ / ↓",
        when: When::Agent,
        effect: "scroll a line",
        double_tap: None,
    },
    Binding {
        chord: "PageUp / PageDown",
        when: When::Agent,
        effect: "scroll a page",
        double_tap: None,
    },
    Binding {
        chord: "Enter",
        when: When::Agent,
        effect: "open or close the focused row",
        double_tap: None,
    },
    Binding {
        chord: "[ / ]",
        when: When::Agent,
        effect: "show the previous or next agent in the Agents list's order",
        double_tap: None,
    },
    Binding {
        chord: "i / c",
        when: When::Agent,
        effect: "ask to interrupt or close this agent, in the confirmation pop-up",
        double_tap: None,
    },
    Binding {
        chord: "x",
        when: When::Agent,
        effect: "acknowledge this agent's attention",
        double_tap: None,
    },
    Binding {
        chord: "y",
        when: When::Agent,
        effect: "copy this agent's id",
        double_tap: None,
    },
    Binding {
        chord: "click agent bar",
        when: When::Agent,
        effect: "back to the Agents list at this agent",
        double_tap: None,
    },
    Binding {
        chord: "← / →",
        when: When::Modal,
        effect: "switch to the previous or next tab",
        double_tap: None,
    },
    Binding {
        chord: "Tab / Shift+Tab",
        when: When::Modal,
        effect: "switch to the next or previous tab",
        double_tap: None,
    },
    Binding {
        chord: "click a tab",
        when: When::Modal,
        effect: "switch to that tab",
        double_tap: None,
    },
    Binding {
        chord: "↑ / ↓",
        when: When::Modal,
        effect: "move the focused row",
        double_tap: None,
    },
    Binding {
        chord: "Space",
        when: When::Modal,
        effect: "toggle the focused switch",
        double_tap: None,
    },
    Binding {
        chord: "Enter",
        when: When::Modal,
        effect: "open a chooser or stepper, or fire the focused action; Providers has none and only focuses",
        double_tap: None,
    },
    Binding {
        chord: "/",
        when: When::Modal,
        effect: "search settings by label, description and keywords",
        double_tap: None,
    },
    Binding {
        chord: "d",
        when: When::Modal,
        effect: "ask to reset the focused row to its schema default, in the confirmation pop-up",
        double_tap: None,
    },
    Binding {
        chord: "any printable key",
        when: When::Modal,
        effect: "filter the Agents list, or the Settings list once / opened the search",
        double_tap: None,
    },
    Binding {
        chord: "Backspace",
        when: When::Modal,
        effect: "delete the last character of that filter",
        double_tap: None,
    },
    Binding {
        chord: "PageUp / PageDown",
        when: When::Modal,
        effect: "scroll the tab body one screen",
        double_tap: None,
    },
    Binding {
        chord: "Home / End",
        when: When::Modal,
        effect: "jump to the top or bottom of the tab body",
        double_tap: None,
    },
    Binding {
        chord: "click a row",
        when: When::Modal,
        effect: "focus it; double-click it to activate it as Enter does",
        double_tap: None,
    },
    Binding {
        chord: "click [\u{00d7}]",
        when: When::Modal,
        effect: "close the Menu",
        double_tap: None,
    },
    Binding {
        chord: "Enter (Keybinds)",
        when: When::Modal,
        effect: "open All keybinds from While typing",
        double_tap: None,
    },
    Binding {
        chord: "y",
        when: When::Modal,
        effect: "copy the Providers, Usage, Context or Session tab to the clipboard",
        double_tap: None,
    },
    Binding {
        chord: "Esc",
        when: When::Modal,
        effect: "return from All keybinds, revert a live preview, or close Menu",
        double_tap: None,
    },
    // Agents list (spec §5.7). Every chord here is pinned against the
    // input layer by `tree_bindings_match_the_input_layer` in `ui`.
    Binding {
        chord: "↑ / k",
        when: When::Tree,
        effect: "select the previous visible row",
        double_tap: None,
    },
    Binding {
        chord: "↓ / j",
        when: When::Tree,
        effect: "select the next visible row",
        double_tap: None,
    },
    Binding {
        chord: "← / h",
        when: When::Tree,
        effect: "select the parent node",
        double_tap: None,
    },
    Binding {
        chord: "→ / l",
        when: When::Tree,
        effect: "select the last-visited child",
        double_tap: None,
    },
    Binding {
        chord: "Home",
        when: When::Tree,
        effect: "select the root",
        double_tap: None,
    },
    Binding {
        chord: "Enter",
        when: When::Tree,
        effect: "show the selected agent's transcript in Activity; on root, close Activity and show the root transcript",
        double_tap: None,
    },
    Binding {
        chord: "f",
        when: When::Tree,
        effect: "switch between the live working set and every agent",
        double_tap: None,
    },
    Binding {
        chord: "x",
        when: When::Tree,
        effect: "acknowledge the selected agent's attention",
        double_tap: None,
    },
    Binding {
        chord: "y",
        when: When::Tree,
        effect: "copy the selected agent's id",
        double_tap: None,
    },
    Binding {
        chord: "R",
        when: When::Tree,
        effect: "ask the host for a fresh tree snapshot",
        double_tap: None,
    },
    Binding {
        chord: "r",
        when: When::Tree,
        effect: "ask to resume the restored-paused tree — the only exit from it — in the confirmation pop-up",
        double_tap: None,
    },
    Binding {
        chord: "Tab / Shift+Tab",
        when: When::Tree,
        effect: "switch to Activity Processes",
        double_tap: None,
    },
    Binding {
        chord: "Tab / Shift+Tab",
        when: When::Processes,
        effect: "switch to Activity Agents",
        double_tap: None,
    },
    Binding {
        chord: "↑ / k",
        when: When::Processes,
        effect: "select previous process or scroll output up",
        double_tap: None,
    },
    Binding {
        chord: "↓ / j",
        when: When::Processes,
        effect: "select next process or scroll output down",
        double_tap: None,
    },
    Binding {
        chord: "Enter",
        when: When::Processes,
        effect: "open selected process output",
        double_tap: None,
    },
    Binding {
        chord: "s",
        when: When::Processes,
        effect: "ask to stop the selected process, in the confirmation pop-up",
        double_tap: None,
    },
    Binding {
        chord: "S",
        when: When::Processes,
        effect: "ask to stop all background processes, in the confirmation pop-up",
        double_tap: None,
    },
    Binding {
        chord: "PageUp / PageDown",
        when: When::Processes,
        effect: "page process output",
        double_tap: None,
    },
    Binding {
        chord: "Home / End",
        when: When::Processes,
        effect: "jump to first or last process/output row",
        double_tap: None,
    },
    Binding {
        chord: "Esc",
        when: When::Processes,
        effect: "close output or Activity",
        double_tap: None,
    },
    // The destructive confirmation pop-up (stop, interrupt, close, resume,
    // quit over processes, apply a profile, reset a setting).
    Binding {
        chord: "← / → / Tab",
        when: When::Confirm,
        effect: "move focus between the buttons at once; focus starts on the action",
        double_tap: None,
    },
    Binding {
        chord: "Enter",
        when: When::Confirm,
        effect: "press the focused button; over the composer a letter closes into the draft",
        double_tap: None,
    },
    Binding {
        chord: "Esc",
        when: When::Confirm,
        effect: "cancel the pop-up, as does a click outside it",
        double_tap: None,
    },
    Binding {
        chord: "i",
        when: When::Tree,
        effect: "ask to interrupt the selected agent, in the confirmation pop-up",
        double_tap: None,
    },
    Binding {
        chord: "c",
        when: When::Tree,
        effect: "ask to close the selected agent, in the confirmation pop-up",
        double_tap: None,
    },
    Binding {
        chord: "s",
        when: When::Tree,
        effect: "ask to stop every non-root agent, in the confirmation pop-up",
        double_tap: None,
    },
    Binding {
        chord: "Esc",
        when: When::Tree,
        effect: "dismiss Activity — does not cancel the root turn",
        double_tap: None,
    },
    // Resume picker (`/resume`, and the launch picker).
    Binding {
        chord: "↑",
        when: When::Resume,
        effect: "select the previous session",
        double_tap: None,
    },
    Binding {
        chord: "↓",
        when: When::Resume,
        effect: "select the next session",
        double_tap: None,
    },
    Binding {
        chord: "Enter",
        when: When::Resume,
        effect: "resume the selected session",
        double_tap: None,
    },
    Binding {
        chord: "Esc",
        when: When::Resume,
        effect: "decline the picker and keep the current session",
        double_tap: None,
    },
    // Floating text overlay (`/help`).
    Binding {
        chord: "↑ / ↓",
        when: When::Overlay,
        effect: "scroll the overlay one line",
        double_tap: None,
    },
    Binding {
        chord: "PageUp / PageDown",
        when: When::Overlay,
        effect: "scroll the overlay one screen",
        double_tap: None,
    },
    Binding {
        chord: "Home / End",
        when: When::Overlay,
        effect: "jump to the top or bottom of the overlay",
        double_tap: None,
    },
    Binding {
        chord: "Enter / q / Esc",
        when: When::Overlay,
        effect: "close the overlay; a click anywhere closes it too",
        double_tap: None,
    },
];

/// Bindings live in `when`, in registration order.
#[must_use]
pub fn bindings_for(when: When) -> Vec<&'static Binding> {
    BINDINGS.iter().filter(|entry| entry.when == when).collect()
}

/// Chord column width used by the generated sheet.
/// Column the sheet's description text starts at. The renderer wraps a long
/// description under this column, so it is public to that one caller rather
/// than re-derived from the rendered string.
pub const CHORD_WIDTH: usize = 24;

/// Chord prefix of the Command-key (macOS) / Super-key bindings, which
/// While typing folds into [`CMD_SUPER_ROW`].
const CMD_SUPER: &str = "Cmd/Super+";

/// While typing's one row for every [`CMD_SUPER`] chord.
const CMD_SUPER_ROW: &str = "copy / cut / select all / undo / redo";

/// Rows While typing leaves to All keybinds: readline extras and a legacy
/// terminal's twin of a chord the page already lists.
const TYPING_OMITS: &[&str] = &["Alt+O", "Ctrl+Shift+A", "Alt+D", "Alt+B / Alt+F"];

/// The default Keybinds page: bindings available while typing, not gestures.
#[must_use]
pub fn typing_text() -> String {
    let mut lines = vec!["While typing".to_owned(), String::new()];
    for when in [When::Global, When::Composer] {
        lines.push(when.heading().to_owned());
        if when == When::Global {
            let label = CMD_SUPER.trim_end_matches('+');
            lines.push(format!("{label:<CHORD_WIDTH$}{CMD_SUPER_ROW}"));
        }
        for binding in bindings_for(when) {
            if binding.chord == "any printable key"
                || binding.chord.starts_with(CMD_SUPER)
                || TYPING_OMITS.contains(&binding.chord)
            {
                continue;
            }
            lines.push(binding_line(binding));
        }
        lines.push(String::new());
    }
    lines.push(format!("{:<CHORD_WIDTH$}All keybinds", "Enter"));
    lines.join("\n")
}

fn binding_line(binding: &Binding) -> String {
    let window = binding.double_tap.map_or_else(String::new, |repeat| {
        format!(" ({} ms)", repeat.window.as_millis())
    });
    format!("{:<CHORD_WIDTH$}{}{window}", binding.chord, binding.effect)
}

/// The read-only Keybinds sheet, generated from [`BINDINGS`] (spec 02 §5.2).
#[must_use]
pub fn sheet_lines() -> Vec<String> {
    let mut lines = Vec::new();
    for when in When::ALL {
        let group = bindings_for(when);
        if group.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(when.heading().to_owned());
        for binding in group {
            lines.push(binding_line(binding));
        }
    }
    lines
}

/// The generated sheet as one string.
#[must_use]
pub fn sheet_text() -> String {
    sheet_lines().join("\n")
}

/// Every chord printed on the sheet, in sheet order. Used by the parity test
/// and by the Keybinds tab's own scroll measurement.
#[must_use]
#[cfg(test)]
pub fn sheet_chords() -> Vec<(When, String)> {
    let mut chords = Vec::new();
    for when in When::ALL {
        for binding in bindings_for(when) {
            chords.push((when, binding.chord.to_owned()));
        }
    }
    chords
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod tests {
    #[test]
    fn empty_draft_affordances_have_their_actual_context_and_condition() {
        let question = BINDINGS
            .iter()
            .find(|binding| binding.chord == "?")
            .unwrap();
        assert_eq!(question.when, When::Composer);
        let slash = BINDINGS
            .iter()
            .find(|binding| binding.chord == "/")
            .unwrap();
        assert!(slash.effect.contains("from an empty draft"));
    }

    use super::{
        BINDINGS, CHORD_WIDTH, When, bindings_for, sheet_chords, sheet_lines, sheet_text,
        typing_text,
    };
    use crate::tui::quit_guard::{ESC_CLEAR_WINDOW, QUIT_CONFIRM_WINDOW};

    /// Chords recovered from the rendered sheet, so the assertion below reads
    /// the sheet the operator reads rather than the table behind it.
    fn chords_printed_on_the_sheet() -> Vec<(When, String)> {
        let headings: Vec<(When, &str)> = When::ALL.map(|when| (when, when.heading())).to_vec();
        let mut context = None;
        let mut found = Vec::new();
        for line in sheet_lines() {
            if line.is_empty() {
                continue;
            }
            if let Some((when, _)) = headings.iter().find(|(_, heading)| *heading == line) {
                context = Some(*when);
                continue;
            }
            let when = context.expect("every sheet row sits under a context heading");
            let chord: String = line
                .chars()
                .take(CHORD_WIDTH)
                .collect::<String>()
                .trim_end()
                .to_owned();
            found.push((when, chord));
        }
        found
    }

    #[test]
    fn the_sheet_and_the_registry_agree_exactly() {
        let printed = chords_printed_on_the_sheet();
        let registered = sheet_chords();
        for entry in &registered {
            assert!(
                printed.contains(entry),
                "{entry:?} is in the registry but not on the sheet"
            );
        }
        for entry in &printed {
            assert!(
                registered.contains(entry),
                "{entry:?} is on the sheet but not in the registry"
            );
        }
        assert_eq!(printed.len(), registered.len());
        assert_eq!(printed.len(), BINDINGS.len());
    }

    #[test]
    fn ctrl_x_states_its_dual_role_and_ctrl_dot_is_gone() {
        let sheet = sheet_text();
        assert!(sheet.contains("Ctrl+X"));
        assert!(
            sheet.contains("cut the composer selection")
                && sheet.contains("with no selection reopen Menu on the last-used tab"),
            "spec 02 §5.2: the disambiguation is stated, never implicit:\n{sheet}"
        );
        assert!(!sheet.contains("Ctrl+."), "Ctrl+. is removed with no alias");
    }

    #[test]
    fn double_taps_publish_the_windows_the_input_layer_uses() {
        let esc = BINDINGS
            .iter()
            .find(|binding| binding.chord == "Esc Esc")
            .expect("Esc Esc is registered");
        assert_eq!(esc.double_tap.map(|tap| tap.window), Some(ESC_CLEAR_WINDOW));
        let ctrl_c = BINDINGS
            .iter()
            .find(|binding| binding.chord == "Ctrl+C")
            .expect("Ctrl+C is registered");
        assert_eq!(
            ctrl_c.double_tap.map(|tap| tap.window),
            Some(QUIT_CONFIRM_WINDOW)
        );
        assert!(sheet_text().contains("800 ms"));
        assert!(sheet_text().contains("1000 ms"));
    }

    /// While typing is the short page: the Cmd/Super chords fold into one
    /// row, readline extras and legacy-terminal twins stay on All keybinds,
    /// and nothing it prints is outside the registry.
    #[test]
    fn while_typing_is_short_and_folds_the_cmd_chords() {
        let typing = typing_text();
        let rows: Vec<&str> = typing
            .lines()
            .filter(|line| line.len() > CHORD_WIDTH && !line.ends_with("All keybinds"))
            .collect();
        assert!(rows.len() <= 23, "{} rows:\n{typing}", rows.len());
        assert!(
            typing.contains("Cmd/Super               copy / cut / select all / undo / redo"),
            "{typing}"
        );
        assert!(!typing.contains("Cmd/Super+"), "{typing}");
        for omitted in ["Alt+O", "Ctrl+Shift+A", "Alt+D", "Alt+B / Alt+F"] {
            assert!(
                !rows
                    .iter()
                    .any(|row| row.starts_with(&format!("{omitted} "))),
                "{omitted} belongs on All keybinds only:\n{typing}"
            );
            assert!(sheet_text().contains(omitted));
        }
        let sheet = sheet_text();
        for chord in ["Cmd/Super+C", "Cmd/Super+X", "Cmd/Super+A", "Cmd/Super+Z"] {
            assert!(sheet.contains(chord), "{chord} missing:\n{sheet}");
        }
        assert!(!sheet.contains("SUPER"), "{sheet}");
    }

    /// The Agents list rows say what the key does: `y` copies the agent id,
    /// `x` acknowledges any attention, not only failures.
    #[test]
    fn agents_list_copy_and_acknowledge_name_their_real_effects() {
        let effect = |chord: &str| {
            bindings_for(When::Tree)
                .into_iter()
                .find(|binding| binding.chord == chord)
                .map(|binding| binding.effect)
                .unwrap_or_else(|| panic!("{chord} is not an Agents list binding"))
        };
        assert_eq!(effect("y"), "copy the selected agent's id");
        assert_eq!(effect("x"), "acknowledge the selected agent's attention");
    }

    #[test]
    fn every_context_is_populated_and_every_effect_is_prose() {
        for when in When::ALL {
            assert!(
                !bindings_for(when).is_empty(),
                "{when:?} has no bindings on the sheet"
            );
        }
        for binding in BINDINGS {
            assert_ne!(binding.effect, "");
            assert!(
                binding.chord.chars().count() < CHORD_WIDTH,
                "{} overflows the chord column",
                binding.chord
            );
        }
    }
}
