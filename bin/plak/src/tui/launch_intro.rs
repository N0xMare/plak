//! Session banner: a six-row pixel PLAK wordmark as the first rows of the
//! transcript.
//!
//! Wordmark on the left, build / binary / version / session / `powered by plakous` on
//! the right, a horizontal rule above and below. L and K are the retained
//! figlet art. P and A are generated from cell masks, with a double-line
//! outline hugging the right and bottom edge of every stroke. Ordinary cell
//! text so it paints in tmux and herdr. The composer already carries model and
//! workspace; this strip does not repeat them. The mark is document content at
//! the top of the session: it scrolls away once the transcript fills the pane.

use std::sync::OnceLock;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Cell mask for P: one character per display column, `#` is ink. The rendered
/// block is one row taller and one column wider, for the outline.
const P_MASK: [&str; MASK_ROWS] = ["########", "##....##", "######..", "##......", "##......"];

/// Cell mask for A. Its strokes are inset, so the outline turns inside the
/// glyph as well as along its right edge.
const A_MASK: [&str; MASK_ROWS] = [
    "##########",
    " ##    ## ",
    " ######## ",
    " ##    ## ",
    "####  ####",
];

/// L, kept verbatim from the figlet mark it replaces.
const L_ROWS: [&str; MARK_ROWS] = [
    "██╗     ",
    "██║     ",
    "██║     ",
    "██║     ",
    "███████╗",
    "╚══════╝",
];

/// K, kept verbatim from the figlet mark it replaces.
const K_ROWS: [&str; MARK_ROWS] = [
    "██╗  ██╗",
    "██║ ██╔╝",
    "█████╔╝ ",
    "██╔═██╗ ",
    "██║  ██╗",
    "╚═╝  ╚═╝",
];

const MASK_ROWS: usize = 5;
/// Ink rows plus the outline row below them.
const MARK_ROWS: usize = MASK_ROWS + 1;
/// The mark starts below the top rule.
const MARK_TOP: usize = 1;

const INK_CELL: char = '█';
const RULE_CELL: char = '─';

pub(crate) const BANNER_HEIGHT: u16 = 8;
const WORDMARK_WIDTH: usize = 39;
const LEFT_PAD: usize = 1;
const FACT_GAP: usize = 2;
const FACT_COL: usize = LEFT_PAD + WORDMARK_WIDTH + FACT_GAP;
const MIN_WIDTH: u16 = (LEFT_PAD + WORDMARK_WIDTH) as u16;

const TAG: &str = "powered by plakous";

const INK_ON_DARK: Color = Color::Rgb(236, 228, 214);
const INK_ON_LIGHT: Color = Color::Rgb(28, 24, 20);
const DIM_ON_DARK: Color = Color::Rgb(168, 160, 148);
const DIM_ON_LIGHT: Color = Color::Rgb(88, 82, 74);

/// Facts drawn on the right of the wordmark.
pub(crate) struct BannerFacts<'a> {
    pub light: bool,
    pub version: &'a str,
    pub session: Option<&'a str>,
    pub binary_id: Option<&'a str>,
}

/// Armed session banner. Dropping it is what lets the transcript go flush-top.
pub(crate) struct LaunchIntro {
    pub(crate) binary_id: Option<&'static str>,
}

impl LaunchIntro {
    pub(crate) fn open() -> Self {
        Self {
            binary_id: crate::build_identity::ready_binary_id(),
        }
    }

    pub(crate) fn refresh_binary_id(&mut self) -> bool {
        let ready = crate::build_identity::ready_binary_id();
        if self.binary_id == ready {
            return false;
        }
        self.binary_id = ready;
        true
    }
}

/// True when the transcript pane can hold the mark as session content.
#[must_use]
pub(crate) fn fits(area: Rect) -> bool {
    area.width >= MIN_WIDTH && area.height >= BANNER_HEIGHT
}

/// Paint a window of the banner into `area`. `skip_rows` drops rows above the
/// viewport so the mark can scroll off like the rest of the session.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, skip_rows: usize, facts: BannerFacts<'_>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (ink, dim) = if facts.light {
        (INK_ON_LIGHT, DIM_ON_LIGHT)
    } else {
        (INK_ON_DARK, DIM_ON_DARK)
    };
    let height = usize::from(area.height);
    let lines: Vec<Line<'static>> = banner_rows(
        usize::from(area.width),
        facts.version,
        facts.session,
        facts.binary_id,
    )
    .into_iter()
    .skip(skip_rows)
    .take(height)
    .map(|row| style_row(&row, ink, dim))
    .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Ink cells carry the identity colour; outline strokes, the rules and the
/// facts are dim.
fn style_row(row: &str, ink: Color, dim: Color) -> Line<'static> {
    let ink_style = Style::default().fg(ink);
    let dim_style = Style::default().fg(dim);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_is_ink = false;
    for ch in row.chars() {
        let is_ink = ch == INK_CELL;
        if !run.is_empty() && is_ink != run_is_ink {
            let style = if run_is_ink { ink_style } else { dim_style };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_is_ink = is_ink;
        run.push(ch);
    }
    if !run.is_empty() {
        let style = if run_is_ink { ink_style } else { dim_style };
        spans.push(Span::styled(run, style));
    }
    Line::from(spans)
}

/// True when the mask paints this cell. Masks are ASCII, so bytes index them.
fn mask_ink(mask: &[&str], row: usize, col: usize) -> bool {
    mask.get(row)
        .and_then(|line| line.as_bytes().get(col))
        .is_some_and(|cell| *cell == b'#')
}

/// The outline: every empty cell that touches ink on its left, above, or
/// above-left, so the double line hugs each stroke's right and bottom edge.
fn mask_outline(mask: &[&str], row: usize, col: usize) -> bool {
    if mask_ink(mask, row, col) {
        return false;
    }
    let up = row.checked_sub(1);
    let left = col.checked_sub(1);
    left.is_some_and(|c| mask_ink(mask, row, c))
        || up.is_some_and(|r| mask_ink(mask, r, col))
        || matches!((up, left), (Some(r), Some(c)) if mask_ink(mask, r, c))
}

/// Pick the double-line stroke that joins an outline cell to its neighbours.
fn mask_outline_cell(mask: &[&str], row: usize, col: usize) -> char {
    let up = row
        .checked_sub(1)
        .is_some_and(|r| mask_outline(mask, r, col));
    let down = mask_outline(mask, row + 1, col);
    let left = col
        .checked_sub(1)
        .is_some_and(|c| mask_outline(mask, row, c));
    let right = mask_outline(mask, row, col + 1);
    let ink_left = col.checked_sub(1).is_some_and(|c| mask_ink(mask, row, c));
    let ink_up = row.checked_sub(1).is_some_and(|r| mask_ink(mask, r, col));
    let ink_right = mask_ink(mask, row, col + 1);
    // A stroke's own corners turn before the generic joints get a say, so a
    // lone edge never reads as a tee.
    if ink_left && ink_up && right && !left && !up {
        return '╔';
    }
    if down && !up && !right && ink_left {
        return '╗';
    }
    // A bottom-edge wire that runs into a stroke closes with an upward turn.
    if ink_up && !left && !up && !down && (right || ink_right) {
        return '╚';
    }
    match (up, down, left, right) {
        (true, true, true, true) => '╬',
        (true, true, true, false) => '╣',
        (true, true, false, true) => '╠',
        (true, false, true, true) => '╩',
        (false, true, true, true) => '╦',
        (true, true, _, _) => '║',
        (_, _, true, true) => '═',
        (true, _, true, _) => '╝',
        (true, _, _, true) => '╚',
        (_, true, true, _) => '╗',
        (_, true, _, true) => '╔',
        (true, _, _, _) | (_, true, _, _) => '║',
        _ => '═',
    }
}

/// Render a mask into `MARK_ROWS` rows, one column wider than the mask.
fn mask_block(mask: &[&str]) -> Vec<String> {
    let cols = mask.first().map_or(0, |row| row.len());
    (0..=mask.len())
        .map(|row| {
            (0..=cols)
                .map(|col| {
                    if mask_ink(mask, row, col) {
                        INK_CELL
                    } else if mask_outline(mask, row, col) {
                        mask_outline_cell(mask, row, col)
                    } else {
                        ' '
                    }
                })
                .collect()
        })
        .collect()
}

/// The mark, built once: `MARK_ROWS` rows of `WORDMARK_WIDTH` columns, with a
/// single blank column between letters.
fn wordmark() -> &'static [String] {
    static WORDMARK: OnceLock<Vec<String>> = OnceLock::new();
    WORDMARK.get_or_init(|| {
        let p = mask_block(&P_MASK);
        let a = mask_block(&A_MASK);
        (0..MARK_ROWS)
            .map(|row| format!("{} {} {} {}", p[row], L_ROWS[row], a[row], K_ROWS[row]))
            .collect()
    })
}

fn banner_rows(
    width: usize,
    version: &str,
    session: Option<&str>,
    binary_id: Option<&str>,
) -> Vec<String> {
    let facts = fact_rows(version, session, binary_id);
    let fact_width = facts.iter().map(|row| row.width()).max().unwrap_or(0);
    // Too narrow for the mark plus the gap plus the widest fact: drop the facts
    // and leave the mark left-aligned on its own.
    let show_facts = width >= FACT_COL + fact_width;
    let first_fact = usize::from(BANNER_HEIGHT).saturating_sub(facts.len()) / 2;
    let mark = wordmark();
    let rule: String = std::iter::repeat_n(RULE_CELL, width).collect();
    (0..usize::from(BANNER_HEIGHT))
        .map(|i| {
            // The rows the mark does not reach are the rules that fence it.
            let Some(glyph) = i.checked_sub(MARK_TOP).and_then(|row| mark.get(row)) else {
                return rule.clone();
            };
            let mut row = format!("{}{glyph}", " ".repeat(LEFT_PAD));
            if show_facts && let Some(fact) = i.checked_sub(first_fact).and_then(|at| facts.get(at))
            {
                row = overlay_at(row, FACT_COL, fact, width);
            }
            fit(&row, width)
        })
        .collect()
}

fn fact_rows(version: &str, session: Option<&str>, binary_id: Option<&str>) -> Vec<String> {
    let mut rows = vec![
        format!("{:<7}  {}", "build", crate::build_identity::FLAVOR),
        format!("{:<7}  {}", "binary", binary_id.unwrap_or("calculating")),
        format!("{:<7}  {version}", "version"),
    ];
    if cfg!(feature = "test-hooks") {
        rows.push(format!("{:<7}  test-hooks", "hooks"));
    }
    if let Some(session) = session.filter(|id| !id.is_empty()) {
        let short: String = session.chars().take(8).collect();
        rows.push(format!("{:<7}  {short}", "session"));
    }
    rows.push(TAG.to_owned());
    rows
}

fn overlay_at(mut left: String, col: usize, right: &str, width: usize) -> String {
    let lw = left.width();
    if col >= width || col + 1 >= width {
        return fit(&left, width);
    }
    if lw < col {
        left.push_str(&" ".repeat(col.saturating_sub(lw)));
    } else if lw > col {
        left = fit(&left, col);
    }
    left.push_str(&fit(right, width.saturating_sub(col)));
    left
}

fn fit(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_owned();
    }
    let mut acc = String::new();
    let mut used: usize = 0;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if used.saturating_add(cw) > max {
            break;
        }
        acc.push(ch);
        used = used.saturating_add(cw);
    }
    acc
}

#[cfg(test)]
mod geometry_tests {
    use super::{
        BANNER_HEIGHT, FACT_COL, MARK_ROWS, MARK_TOP, MIN_WIDTH, RULE_CELL, TAG, WORDMARK_WIDTH,
        banner_rows, wordmark,
    };
    use ratatui::layout::Rect;
    use unicode_width::UnicodeWidthStr;

    /// The six-row mark: generated P and A either side of the retained L and K.
    const EXPECTED: [&str; MARK_ROWS] = [
        "████████╗ ██╗      ██████████╗ ██╗  ██╗",
        "██╔═══██║ ██║      ╚██╔═══██╔╝ ██║ ██╔╝",
        "██████╔═╝ ██║       ████████║  █████╔╝ ",
        "██╔═══╝   ██║       ██╔╦══██║  ██╔═██╗ ",
        "██║       ███████╗ ████║ ████╗ ██║  ██╗",
        "╚═╝       ╚══════╝ ╚═══╝ ╚═══╝ ╚═╝  ╚═╝",
    ];

    #[test]
    fn the_wordmark_is_six_rows_of_pixel_letters_with_a_double_line_outline() {
        let rows = wordmark();
        assert_eq!(rows.len(), MARK_ROWS);
        for (got, want) in rows.iter().zip(EXPECTED) {
            assert_eq!(got, want);
        }
        for row in rows {
            assert_eq!(row.width(), WORDMARK_WIDTH, "{row:?}");
        }
    }

    #[test]
    fn the_banner_paints_while_binary_identity_is_pending() {
        let pending = banner_rows(80, "9.8.7-test", None, None);
        assert!(
            pending
                .iter()
                .any(|row| row.contains("binary   calculating"))
        );
        let ready = banner_rows(80, "9.8.7-test", None, Some("012345abcdef"));
        assert!(
            ready
                .iter()
                .any(|row| row.contains("binary   012345abcdef"))
        );
        assert!(ready.iter().all(|row| !row.contains("calculating")));
    }

    #[test]
    fn the_banner_pairs_the_wordmark_with_facts() {
        assert_eq!(BANNER_HEIGHT, 8);
        assert_eq!(WORDMARK_WIDTH, 39);
        assert_eq!(MIN_WIDTH, 40);
        assert_eq!(FACT_COL, 42);
        let rows = banner_rows(80, "9.8.7-test", Some("abcdefghij"), None);
        assert_eq!(rows.len(), usize::from(BANNER_HEIGHT));
        assert!(rows[1].contains("build"), "{rows:?}");
        assert!(rows[2].contains("binary"), "{rows:?}");
        assert!(rows[3].contains("version  9.8.7-test"), "{rows:?}");
        let last_fact = if cfg!(feature = "test-hooks") { 6 } else { 5 };
        assert!(
            rows[last_fact - 1].contains("session  abcdefgh"),
            "{rows:?}"
        );
        assert!(rows[last_fact].contains(TAG), "{rows:?}");
        assert!(rows[last_fact].find(TAG).is_some_and(|at| at >= FACT_COL));
        assert!(!rows[3].contains("no model"));
        for row in &rows {
            assert!(row.width() <= 80, "{row:?}");
        }
        // A full-width rule fences the mark above and below.
        let rule: String = std::iter::repeat_n(RULE_CELL, 80).collect();
        assert_eq!(rows[0], rule, "{:?}", rows[0]);
        assert_eq!(rows[7], rule, "{:?}", rows[7]);
        assert!(
            rows[MARK_TOP].starts_with(&format!(" {}", EXPECTED[0])),
            "{rows:?}"
        );
    }

    #[test]
    fn a_pane_too_narrow_for_the_facts_keeps_the_mark_alone() {
        let rows = banner_rows(45, "9.8.7-test", Some("abcdefghij"), None);
        assert!(rows.iter().all(|row| !row.contains(TAG)), "{rows:?}");
        assert_eq!(rows[MARK_TOP], format!(" {}", EXPECTED[0]), "{rows:?}");
        assert_eq!(rows[0].width(), 45, "the rule still spans the pane");
        assert!(!super::fits(Rect::new(0, 0, 39, 8)));
        assert!(super::fits(Rect::new(0, 0, 40, 8)));
        assert!(!super::fits(Rect::new(0, 0, 80, 7)));
        assert!(super::fits(Rect::new(0, 0, 80, 18)));
    }
}
