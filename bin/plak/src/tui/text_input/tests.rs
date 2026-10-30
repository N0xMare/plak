use super::*;

#[test]
fn control_and_disguising_format_scalars_never_enter_product_text() {
    // Cc: NUL, ESC, BEL, DEL, and the C1 block.
    for ch in ['\u{0}', '\u{1b}', '\u{7}', '\u{7f}', '\u{9b}'] {
        assert!(!is_accepted_input_char(ch), "{ch:?} is Cc");
    }
    // The explicit blocklist: bidi marks, overrides and isolates, the
    // invisible operators, BOM, the line separators, and the deprecated
    // and annotation controls.
    for ch in [
        '\u{ad}',
        '\u{61c}',
        '\u{180e}',
        '\u{200e}',
        '\u{200f}',
        '\u{2028}',
        '\u{2029}',
        '\u{202a}',
        '\u{202c}',
        '\u{202e}',
        '\u{2060}',
        '\u{2064}',
        '\u{2066}',
        '\u{2069}',
        '\u{fff9}',
        '\u{fffb}',
        '\u{feff}',
        '\u{110bd}',
        '\u{110cd}',
        '\u{13430}',
        '\u{1343f}',
        '\u{1bca0}',
        '\u{1bca3}',
        '\u{e0001}',
    ] {
        assert!(!is_accepted_input_char(ch), "{ch:?} must be rejected");
    }
    // Cf is not rejected as a category: joiners, ZWSP and the emoji tag
    // characters are load-bearing in real text.
    for ch in [
        '\u{200b}',
        '\u{200c}',
        '\u{200d}',
        '\u{34f}',
        '\u{fe0e}',
        '\u{fe0f}',
        '\u{e0020}',
        '\u{e007f}',
        '\u{e0100}',
        '\u{e01ef}',
    ] {
        assert!(is_accepted_input_char(ch), "{ch:?} must survive");
    }
    // Tab and newline are the two deliberate composer exceptions; a
    // single-line field takes neither.
    assert!(is_accepted_input_char('\t'));
    assert!(is_accepted_input_char('\n'));
    assert!(!is_accepted_single_line_char('\t'));
    assert!(!is_accepted_single_line_char('\n'));
    assert!(is_accepted_input_char('é'));
    assert!(is_accepted_input_char('日'));
}

#[test]
fn shaping_sequences_survive_paste_and_typing_intact() {
    // Family emoji: four people joined by three ZWJ.
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
    assert_eq!(sanitize_composer_text(family), family);
    assert_eq!(sanitize_single_line(family), family);
    assert!(family.chars().all(is_accepted_input_char));

    // Scotland: a waving flag plus five emoji tag characters.
    let flag = "\u{1f3f4}\u{e0067}\u{e0062}\u{e0073}\u{e0063}\u{e0074}\u{e007f}";
    assert_eq!(sanitize_composer_text(flag), flag);
    assert!(flag.chars().all(is_accepted_input_char));

    // Devanagari with a ZWNJ suppressing the conjunct, and Persian with a
    // ZWNJ holding the word-internal break.
    let devanagari = "\u{915}\u{94d}\u{200c}\u{937}";
    let persian = "\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}";
    for text in [devanagari, persian] {
        assert_eq!(sanitize_composer_text(text), text);
        assert_eq!(sanitize_single_line(text), text);
        assert!(text.chars().all(is_accepted_input_char));
    }

    // A bidi override embedded in the same run is still dropped, and only
    // that scalar is dropped.
    assert_eq!(
        sanitize_composer_text(&format!("{devanagari}\u{202e}{family}")),
        format!("{devanagari}{family}"),
    );
}

#[test]
fn sanitize_drops_controls_and_normalizes_line_endings() {
    assert_eq!(
        sanitize_composer_text("a\u{202e}b\u{0}c"),
        "abc",
        "a rejected scalar is dropped, not escaped"
    );
    assert_eq!(
        sanitize_composer_text("one\r\ntwo\rthree"),
        "one\ntwo\nthree"
    );
    assert_eq!(sanitize_composer_text("keep\tthe\ttabs"), "keep\tthe\ttabs");
    // Single-line fields reject the structural characters too.
    assert_eq!(sanitize_single_line("sk-\tlive\n\u{feff}key"), "sk-livekey");
}
