//! Text-input admission policy shared by every path that can put operator
//! text into the product: keys, bracketed paste, secret fields, modal search,
//! and type-to-filter.
//!
//! One rule, one place (§4 "Composer editing"). Kept out of the `ui` feature so
//! the non-rendering modules can apply the same policy.

/// Format (`Cf`) and line-separator scalars that never enter product text.
///
/// The category as a whole is not the rule: `Cf` also holds the joiners and
/// tag characters that make emoji sequences and Indic/Persian shaping work, so
/// rejecting it wholesale mangles legitimate text. This is the explicit
/// blocklist instead — bidi overrides and isolates, the invisible math
/// operators, the byte-order mark, the deprecated and annotation controls, and
/// the two `Zl`/`Zp` line separators that a terminal would treat as a break.
const REJECTED_FORMAT: &[(char, char)] = &[
    ('\u{00ad}', '\u{00ad}'),   // SOFT HYPHEN
    ('\u{061c}', '\u{061c}'),   // ARABIC LETTER MARK
    ('\u{180e}', '\u{180e}'),   // MONGOLIAN VOWEL SEPARATOR
    ('\u{200e}', '\u{200f}'),   // LRM, RLM
    ('\u{2028}', '\u{2029}'),   // LINE / PARAGRAPH SEPARATOR (Zl, Zp)
    ('\u{202a}', '\u{202e}'),   // LRE, RLE, PDF, LRO, RLO
    ('\u{2060}', '\u{2064}'),   // WORD JOINER + invisible operators
    ('\u{2066}', '\u{2069}'),   // LRI, RLI, FSI, PDI
    ('\u{fff9}', '\u{fffb}'),   // interlinear annotation
    ('\u{feff}', '\u{feff}'),   // ZWNBSP / BOM
    ('\u{110bd}', '\u{110bd}'), // KAITHI NUMBER SIGN
    ('\u{110cd}', '\u{110cd}'), // KAITHI NUMBER SIGN ABOVE
    ('\u{13430}', '\u{1343f}'), // Egyptian hieroglyph format controls
    ('\u{1bca0}', '\u{1bca3}'), // Shorthand format controls
    ('\u{e0001}', '\u{e0001}'), // LANGUAGE TAG
];

/// Whether a character may enter product text from any input path.
///
/// Unicode `Cc` (control) carries terminal escapes into a prompt and is
/// rejected outright; tab and newline are the two deliberate exceptions the
/// composer handles itself (§4 "Composer editing"). `Cf` is *not* rejected as a
/// category: ZWJ (U+200D), ZWNJ (U+200C), ZWSP (U+200B) and the emoji tag
/// characters (U+E0020–U+E007F) are load-bearing in family emoji, subdivision
/// flags, and Devanagari/Persian shaping, and dropping them silently corrupts
/// pasted text. Only [`REJECTED_FORMAT`] — the bidi and invisible controls that
/// can disguise what the operator is about to send — is filtered out.
///
/// Every input path shares this one function: keys, bracketed paste, secret
/// fields, and modal search.
#[must_use]
pub fn is_accepted_input_char(ch: char) -> bool {
    if ch == '\t' || ch == '\n' {
        return true;
    }
    if ch.is_control() {
        return false;
    }
    !REJECTED_FORMAT
        .iter()
        .any(|(low, high)| ch >= *low && ch <= *high)
}

/// Whether a character may enter a single-line field (secret input, modal
/// search, type-to-filter). Newline and tab are structural there, so the two
/// composer exceptions do not apply.
#[must_use]
pub fn is_accepted_single_line_char(ch: char) -> bool {
    ch != '\t' && ch != '\n' && is_accepted_input_char(ch)
}

/// Filter a run of inserted text for the composer.
///
/// `\r\n` and a bare `\r` normalize to `\n`; every scalar rejected by
/// [`is_accepted_input_char`] is dropped rather than escaped, so a rejected
/// paste can never reconstruct a control sequence from surviving neighbours.
#[must_use]
pub fn sanitize_composer_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                let _ = chars.next();
            }
            out.push('\n');
            continue;
        }
        if is_accepted_input_char(ch) {
            out.push(ch);
        }
    }
    out
}

/// Filter a run of inserted text for a single-line field (secret input, modal
/// search). Newline and tab are structural there, so they are rejected too.
#[must_use]
pub fn sanitize_single_line(text: &str) -> String {
    text.chars()
        .filter(|ch| is_accepted_single_line_char(*ch))
        .collect()
}

#[cfg(test)]
mod tests;
