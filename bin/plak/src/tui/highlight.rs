//! Cached syntect highlighting for Compose programs and markdown fences.
//!
//! A fence that is still streaming is highlighted again on every frame with a
//! few more lines. Syntect's state is line by line, so the rows and the parse
//! state after the last complete line are memoized per thread and a later call
//! whose source extends that prefix highlights only the new lines.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::OnceLock;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SyntectColor, FontStyle, HighlightState, ScopeSelectors, Theme as SyntaxTheme,
    ThemeItem, ThemeSettings,
};
use syntect::parsing::{ParseState, SyntaxReference, SyntaxSet};

/// Semantic styles applied to syntect scopes.
///
/// Roles map onto the active product theme; the highlighter itself uses
/// sentinel RGB values so it never assumes a terminal palette.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HighlightStyles {
    /// Unclassified code and the fallback when a line cannot be highlighted.
    pub default: Style,
    /// Comments.
    pub comment: Style,
    /// String literals.
    pub string: Style,
    /// Numbers, booleans, and language constants.
    pub constant: Style,
    /// Keywords and storage modifiers.
    pub keyword: Style,
    /// Function names.
    pub function: Style,
    /// Types, classes, and similar names.
    pub type_name: Style,
    /// Parameters.
    pub parameter: Style,
    /// Invalid or illegal tokens.
    pub invalid: Style,
}

impl Default for HighlightStyles {
    fn default() -> Self {
        Self {
            default: Style::default(),
            comment: Style::default()
                .add_modifier(Modifier::DIM)
                .add_modifier(Modifier::ITALIC),
            string: Style::default(),
            constant: Style::default(),
            keyword: Style::default().add_modifier(Modifier::BOLD),
            function: Style::default(),
            type_name: Style::default(),
            parameter: Style::default().add_modifier(Modifier::ITALIC),
            invalid: Style::default().add_modifier(Modifier::UNDERLINED),
        }
    }
}

struct Assets {
    syntaxes: SyntaxSet,
}

/// A shell line touching the grammar's common contexts: quoting, expansion,
/// redirection, a trap, a loop and a pipe.
const WARM_SAMPLE: &str = "trap 'printf %s \"$1\" > out.txt; exit 0' TERM; while :; do echo \"${HOME}\" | wc -c; sleep 0.1; done";

/// Load the syntax set and compile the shell grammar off the UI thread.
///
/// Both are lazy and, in an unoptimized build on a loaded host, take long
/// enough to stall a frame: the first command row would otherwise pay for
/// them inside a draw, quitting frame included. The warm-up only fills the
/// same process-wide caches a first draw fills, so a draw that arrives while
/// it runs waits at most for the remainder, and one that fails to start
/// leaves the first draw to initialize on demand. A panic in it is no quieter
/// than one in a draw: the TUI's process-wide panic hook still runs and
/// restores the terminal, and the caches stay unset for the next draw.
pub(crate) fn warm_in_background() {
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("plak-highlight-warm".to_owned())
            .spawn(warm);
    });
}

fn warm() {
    let _ = lines(WARM_SAMPLE, Some("bash"), &HighlightStyles::default());
}

fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| Assets {
        syntaxes: SyntaxSet::load_defaults_newlines(),
    })
}

fn syntax_theme() -> &'static SyntaxTheme {
    static THEME: OnceLock<SyntaxTheme> = OnceLock::new();
    THEME.get_or_init(build_syntax_theme)
}

fn build_syntax_theme() -> SyntaxTheme {
    SyntaxTheme {
        name: Some("plak".to_owned()),
        settings: ThemeSettings {
            foreground: Some(sentinel(Role::Default)),
            ..ThemeSettings::default()
        },
        scopes: vec![
            rule("comment", Role::Comment, Some(FontStyle::ITALIC)),
            rule("string", Role::String, None),
            rule(
                "constant.numeric, constant.language, constant.character",
                Role::Constant,
                None,
            ),
            rule("keyword, storage", Role::Keyword, Some(FontStyle::BOLD)),
            rule(
                "entity.name.function, support.function",
                Role::Function,
                None,
            ),
            rule(
                "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, support.type",
                Role::Type,
                None,
            ),
            rule(
                "variable.parameter",
                Role::Parameter,
                Some(FontStyle::ITALIC),
            ),
            rule("invalid", Role::Invalid, Some(FontStyle::UNDERLINE)),
        ],
        ..SyntaxTheme::default()
    }
}

fn syntax_for_token<'a>(syntaxes: &'a SyntaxSet, token: &str) -> &'a SyntaxReference {
    let token = token.split_ascii_whitespace().next().unwrap_or_default();
    syntaxes
        .find_syntax_by_token(token)
        .or_else(|| syntaxes.find_syntax_by_extension(token))
        .or_else(|| syntaxes.find_syntax_by_name(token))
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text())
}

/// Memoized highlights kept per thread: enough for the fences of one streaming
/// answer plus a few Compose programs.
const MEMO_ENTRIES: usize = 8;

/// The highlight of one source's complete lines and the state after them.
struct Memo {
    language: Option<String>,
    styles: HighlightStyles,
    /// The complete lines already highlighted, each with its `\n`.
    done: String,
    rows: Vec<Vec<Span<'static>>>,
    highlight: HighlightState,
    parse: ParseState,
}

thread_local! {
    static MEMO: RefCell<VecDeque<Memo>> = const { RefCell::new(VecDeque::new()) };
}

/// Highlight `source` as `language`, one vector of spans per source line.
///
/// `language` is a fence info token (`javascript`, `python`, `rust`, …).
/// `None` or an unknown token uses plain text, still returning one row per
/// line so callers can paint rails without a second wrap pass. A source that
/// extends one highlighted before resumes after its last complete line; the
/// result is the same as highlighting from the start.
#[must_use]
pub fn lines(
    source: &str,
    language: Option<&str>,
    styles: &HighlightStyles,
) -> Vec<Vec<Span<'static>>> {
    let assets = assets();
    let trimmed = source.trim_end_matches('\n');
    if trimmed.is_empty() && source.is_empty() {
        return Vec::new();
    }
    let theme = syntax_theme();
    let resumed = MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        let position = memo.iter().position(|entry| {
            entry.language.as_deref() == language
                && entry.styles == *styles
                && entry.done.len() <= trimmed.len()
                && trimmed.starts_with(entry.done.as_str())
        })?;
        memo.remove(position)
    });
    let mut entry = resumed.unwrap_or_else(|| {
        let syntax = language.map_or_else(
            || assets.syntaxes.find_syntax_plain_text(),
            |token| syntax_for_token(&assets.syntaxes, token),
        );
        let fresh = HighlightLines::new(syntax, theme);
        let (highlight, parse) = fresh.state();
        Memo {
            language: language.map(str::to_owned),
            styles: *styles,
            done: String::new(),
            rows: Vec::new(),
            highlight,
            parse,
        }
    });
    let mut highlighter = HighlightLines::from_state(theme, entry.highlight, entry.parse);
    let rest = &trimmed[entry.done.len()..];
    let (complete, last) = rest
        .rsplit_once('\n')
        .map_or(("", rest), |(complete, last)| {
            (&rest[..=complete.len()], last)
        });
    for line in complete.split_terminator('\n') {
        entry.rows.push(highlight_line(
            &mut highlighter,
            line,
            &assets.syntaxes,
            styles,
        ));
    }
    entry.done.push_str(complete);
    let (highlight, parse) = highlighter.state();
    let mut rows = entry.rows.clone();
    let mut tail = HighlightLines::from_state(theme, highlight.clone(), parse.clone());
    rows.push(highlight_line(&mut tail, last, &assets.syntaxes, styles));
    entry.highlight = highlight;
    entry.parse = parse;
    MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        memo.push_front(entry);
        memo.truncate(MEMO_ENTRIES);
    });
    rows
}

fn highlight_line(
    highlighter: &mut HighlightLines<'_>,
    text: &str,
    syntaxes: &SyntaxSet,
    styles: &HighlightStyles,
) -> Vec<Span<'static>> {
    let line = format!("{text}\n");
    let Ok(regions) = highlighter.highlight_line(&line, syntaxes) else {
        return vec![Span::styled(text.to_owned(), styles.default)];
    };
    let mut spans = Vec::new();
    for (style, region) in regions {
        let region = region.trim_end_matches('\n');
        if region.is_empty() {
            continue;
        }
        spans.push(Span::styled(
            region.to_owned(),
            style_for(style.foreground, styles),
        ));
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), styles.default));
    }
    spans
}

#[derive(Clone, Copy)]
enum Role {
    Default,
    Comment,
    String,
    Constant,
    Keyword,
    Function,
    Type,
    Parameter,
    Invalid,
}

fn sentinel(role: Role) -> SyntectColor {
    let r = match role {
        Role::Default => 1,
        Role::Comment => 2,
        Role::String => 3,
        Role::Constant => 4,
        Role::Keyword => 5,
        Role::Function => 6,
        Role::Type => 7,
        Role::Parameter => 8,
        Role::Invalid => 9,
    };
    SyntectColor {
        r,
        g: 0,
        b: 0,
        a: 0xff,
    }
}

fn style_for(color: SyntectColor, styles: &HighlightStyles) -> Style {
    match color.r {
        2 => styles.comment,
        3 => styles.string,
        4 => styles.constant,
        5 => styles.keyword,
        6 => styles.function,
        7 => styles.type_name,
        8 => styles.parameter,
        9 => styles.invalid,
        _ => styles.default,
    }
}

fn rule(scope: &str, role: Role, font_style: Option<FontStyle>) -> ThemeItem {
    ThemeItem {
        scope: ScopeSelectors::from_str(scope).unwrap_or_else(|_| ScopeSelectors {
            selectors: Vec::new(),
        }),
        style: syntect::highlighting::StyleModifier {
            foreground: Some(sentinel(role)),
            background: None,
            font_style,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{HighlightStyles, lines, warm};
    use ratatui::style::Modifier;

    /// Warming shares only the process-wide caches: a command highlighted on a
    /// fresh thread after the warm-up is identical to one highlighted before.
    #[test]
    fn warming_changes_no_highlighted_line() {
        let command =
            "trap 'printf stopped > stopped.txt; exit 0' TERM\nwhile :; do sleep 0.1; done\n";
        let highlight = || {
            std::thread::spawn(move || lines(command, Some("bash"), &HighlightStyles::default()))
                .join()
                .unwrap()
        };
        let before = highlight();
        std::thread::spawn(warm).join().unwrap();
        assert_eq!(highlight(), before);
    }

    #[test]
    fn javascript_keywords_are_distinct_from_the_rest_of_the_line() {
        let styles = HighlightStyles::default();
        let rows = lines("const greeting = \"hello\";", Some("javascript"), &styles);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0].iter().any(|span| {
                span.content.as_ref() == "const" && span.style.add_modifier.contains(Modifier::BOLD)
            }),
            "javascript keywords should be their own bold span: {rows:?}"
        );
        assert!(
            rows[0].iter().any(|span| {
                span.content.as_ref().contains("hello")
                    && !span.style.add_modifier.contains(Modifier::BOLD)
            }),
            "string contents should not use the keyword style: {rows:?}"
        );
    }

    /// A fence streamed line by line resumes from its memoized state, and every
    /// intermediate result equals highlighting the same source from scratch.
    #[test]
    fn incremental_highlighting_equals_a_fresh_highlight() {
        let styles = HighlightStyles::default();
        let program = "/* a block comment\nspanning lines */\nfn main() {\n    let s = \"multi\nline string\";\n\n    println!(\"{s}\");\n}\n";
        let other = "def f():\n    \"\"\"doc\n    string\"\"\"\n    return 1\n";
        let fresh = |source: &str, language: &str| {
            super::MEMO.with(|memo| memo.borrow_mut().clear());
            lines(source, Some(language), &styles)
        };
        let mut expected = Vec::new();
        for (source, language) in [(program, "rust"), (other, "python")] {
            for end in 0..=source.len() {
                expected.push((&source[..end], language, fresh(&source[..end], language)));
            }
        }
        super::MEMO.with(|memo| memo.borrow_mut().clear());
        // Interleave the two streams the way two fences in one answer are
        // highlighted within one frame.
        let (rust, python): (Vec<_>, Vec<_>) = expected
            .iter()
            .partition(|(_, language, _)| *language == "rust");
        for step in 0..rust.len().max(python.len()) {
            for stream in [&rust, &python] {
                if let Some((source, language, rows)) = stream.get(step) {
                    assert_eq!(&lines(source, Some(language), &styles), rows, "{source:?}");
                }
            }
        }
    }

    #[test]
    fn python_def_is_a_keyword_span() {
        let styles = HighlightStyles::default();
        let rows = lines("def greet(name):\n    return name", Some("python"), &styles);
        assert!(rows.len() >= 2, "{rows:?}");
        assert!(
            rows[0].iter().any(|span| {
                span.content.as_ref() == "def" && span.style.add_modifier.contains(Modifier::BOLD)
            }),
            "python def should be its own bold span: {rows:?}"
        );
    }
}
