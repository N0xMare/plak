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
    BINDINGS, CHORD_WIDTH, When, bindings_for, sheet_chords, sheet_lines, sheet_text, typing_text,
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
    assert!(rows.len() <= 24, "{} rows:\n{typing}", rows.len());
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
