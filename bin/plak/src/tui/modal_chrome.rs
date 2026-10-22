//! The modal frame (spec 02 §5.5).
//!
//! Menu and Activity share one full-page frame: the whole terminal, the same
//! title row, tab row, footer and close glyph. The resume picker stays a
//! centered modal. The chrome owns the rounded border, the title on the
//! border, the clickable `[×]` close glyph, the wrapping tab bar, the wrapping
//! centered footer, the scrim, and the scroll state every tab shares. A tab
//! body receives a rect and never draws its own border. Menu uses a wide
//! list/detail split when the usable inner column can fit both panes.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::{Frame, style::Color};

use unicode_width::UnicodeWidthStr;

use crate::tui::theme::Theme;

/// Hard floor below which the modal refuses to draw (spec 02 §5.5).
pub(crate) const MODAL_MIN_WIDTH: u16 = 20;
/// Hard floor below which the modal refuses to draw (spec 02 §5.5).
pub(crate) const MODAL_MIN_HEIGHT: u16 = 6;
/// Terminal height at or below which compact sizing applies.
pub(crate) const COMPACT_HEIGHT: u16 = 26;
/// Terminal width at or below which the modal drops to narrow rules.
pub(crate) const NARROW_WIDTH: u16 = 60;

/// Centered-modal geometry. Percentages are integer tenths of a percent-free
/// fraction expressed as `width_num / width_den` so the type stays `const`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ModalSizing {
    /// Numerator of the terminal-width fraction the modal occupies.
    pub width_num: u16,
    /// Denominator of the terminal-width fraction the modal occupies.
    pub width_den: u16,
    /// Upper width clamp.
    pub max_width: u16,
    /// Lower width clamp, before the terminal itself clamps.
    pub min_width: u16,
    /// Rows of margin above and below.
    pub v_margin: u16,
    /// Columns of padding inside the border on each side.
    pub h_pad: u16,
    /// Rows of padding inside the border, above the body.
    pub v_pad: u16,
    /// The whole terminal at every size: the compact and narrow rules do
    /// not apply.
    pub full_screen: bool,
}

impl ModalSizing {
    /// Menu and Activity: the whole terminal above the refuse-to-draw floor.
    /// Compact and narrow rules do not apply; one column of side padding.
    pub(crate) const fn full_page() -> Self {
        Self {
            width_num: 1,
            width_den: 1,
            max_width: u16::MAX,
            min_width: 0,
            v_margin: 0,
            h_pad: 1,
            v_pad: 0,
            full_screen: true,
        }
    }

    /// The resume picker's sizing: narrower than the Menu, because a
    /// row is one id, one age, one line of prompt and one binding rather than a
    /// settings sheet.
    pub(crate) const fn resume_picker() -> Self {
        Self {
            width_num: 13,
            width_den: 20,
            max_width: 120,
            min_width: 48,
            v_margin: 4,
            h_pad: 2,
            v_pad: 1,
            full_screen: false,
        }
    }

    /// Compact rules: no vertical margin or padding, one column of side padding
    /// so a selection band still has a gutter.
    pub(crate) const fn compact(self) -> Self {
        Self {
            v_margin: 0,
            h_pad: 1,
            v_pad: 0,
            ..self
        }
    }

    /// Narrow rules: give up the width fraction and the side padding rather
    /// than truncating content.
    pub(crate) const fn narrow(self) -> Self {
        Self {
            width_num: 1,
            width_den: 1,
            min_width: MODAL_MIN_WIDTH,
            h_pad: 1,
            ..self
        }
    }

    /// Sizing adapted to `area`, applying the compact and narrow rules.
    pub(crate) const fn for_area(self, area: Rect) -> Self {
        if self.full_screen {
            return self;
        }
        let sizing = if area.width <= NARROW_WIDTH {
            self.narrow()
        } else {
            self
        };
        if area.height <= COMPACT_HEIGHT {
            sizing.compact()
        } else {
            sizing
        }
    }

    /// Centered popup rect, or `None` below the refuse-to-draw floor.
    pub(crate) fn popup(self, area: Rect) -> Option<Rect> {
        if area.width < MODAL_MIN_WIDTH || area.height < MODAL_MIN_HEIGHT {
            return None;
        }
        let sizing = self.for_area(area);
        if sizing.full_screen {
            return Some(area);
        }
        let ceiling = area.width.saturating_sub(2).min(sizing.max_width);
        let fraction = area
            .width
            .saturating_mul(sizing.width_num)
            .checked_div(sizing.width_den)
            .unwrap_or(area.width);
        let width = fraction
            .min(ceiling.max(sizing.min_width.min(area.width)))
            .max(sizing.min_width.min(area.width))
            .min(area.width);
        let height = area
            .height
            .saturating_sub(sizing.v_margin.saturating_mul(2))
            .max(MODAL_MIN_HEIGHT.min(area.height))
            .min(area.height);
        if width < MODAL_MIN_WIDTH || height < MODAL_MIN_HEIGHT {
            return None;
        }
        Some(Rect {
            x: area.x.saturating_add(area.width.saturating_sub(width) / 2),
            y: area
                .y
                .saturating_add(area.height.saturating_sub(height) / 2),
            width,
            height,
        })
    }
}

/// Everything a tab body needs after the chrome has drawn itself.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ModalLayout {
    /// The full modal rect, used for outside-click close.
    pub popup: Rect,
    /// Clickable close-glyph rect on the top border.
    pub close: Rect,
    /// One rect per tab index, in declaration order. A tab that could not be
    /// drawn is absent, so a stale click cannot fire it.
    pub tabs: Vec<(usize, Rect)>,
    /// Body area handed to the active tab.
    pub body: Rect,
    /// Footer hint rows.
    pub footer: Rect,
}

/// One footer hint. `key` is the chord; `effect` names what it does, never the
/// key's generic meaning (spec 02 §5.5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Hint {
    /// Chord text, e.g. `Space`.
    pub key: &'static str,
    /// Effect text, e.g. `disable`.
    pub effect: &'static str,
    /// Something outside the modal waits on the operator: drawn
    /// error-coloured as `effect · key`.
    pub alert: bool,
}

impl Hint {
    /// Builds a hint.
    pub(crate) const fn new(key: &'static str, effect: &'static str) -> Self {
        Self {
            key,
            effect,
            alert: false,
        }
    }

    /// An alert hint: `root needs approval · Esc`.
    pub(crate) const fn alert(key: &'static str, effect: &'static str) -> Self {
        Self {
            key,
            effect,
            alert: true,
        }
    }

    fn width(self) -> usize {
        let gap = if self.alert {
            HINT_SEPARATOR.chars().count()
        } else {
            1
        };
        self.key.chars().count() + gap + self.effect.chars().count()
    }
}

const HINT_SEPARATOR: &str = " · ";

/// One line, the same text [`render`] centers in the footer.
fn hint_row_text(line: &[Hint]) -> String {
    let mut parts = Vec::with_capacity(line.len());
    for hint in line {
        if hint.alert {
            parts.push(format!("{}{HINT_SEPARATOR}{}", hint.effect, hint.key));
        } else {
            parts.push(format!("{} {}", hint.key, hint.effect));
        }
    }
    parts.join(HINT_SEPARATOR)
}

/// Footer rows as painted: `(x, y, text)` with `x` the centered origin.
pub(crate) fn footer_lines(footer: Rect, hints: &[Hint]) -> Vec<(u16, u16, String)> {
    let mut lines = Vec::new();
    if footer.width == 0 || footer.height == 0 {
        return lines;
    }
    for (row, line) in wrap_hints(hints, footer.width).into_iter().enumerate() {
        let Ok(row) = u16::try_from(row) else {
            continue;
        };
        if row >= footer.height {
            break;
        }
        let text = hint_row_text(&line);
        let slack = usize::from(footer.width).saturating_sub(text.width());
        let x = footer
            .x
            .saturating_add(u16::try_from(slack / 2).unwrap_or(0));
        lines.push((x, footer.y.saturating_add(row), text));
    }
    lines
}

/// Below the modal floor: one line, the longest of the three that fits.
pub(crate) fn draw_too_small(frame: &mut Frame<'_>, area: Rect, theme: &Theme, title: &str) {
    if area.height == 0 {
        return;
    }
    let notices = [
        format!("{title} · terminal too small"),
        "terminal too small".to_owned(),
        "too small".to_owned(),
    ];
    let width = usize::from(area.width);
    let notice = notices
        .into_iter()
        .find(|notice| notice.width() <= width)
        .unwrap_or_default();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(notice, theme.warning))),
        Rect::new(area.x, area.y, area.width, 1),
    );
}

/// Greedily wraps hints into centered rows that fit `width`.
pub(crate) fn wrap_hints(hints: &[Hint], width: u16) -> Vec<Vec<Hint>> {
    let width = usize::from(width);
    let mut rows: Vec<Vec<Hint>> = Vec::new();
    let mut row: Vec<Hint> = Vec::new();
    let mut used = 0usize;
    for hint in hints {
        let cost = hint.width()
            + if row.is_empty() {
                0
            } else {
                HINT_SEPARATOR.len()
            };
        if !row.is_empty() && used + cost > width {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        used += hint.width()
            + if row.is_empty() {
                0
            } else {
                HINT_SEPARATOR.len()
            };
        row.push(*hint);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// Shared vertical scroll state. Every tab owns one (spec 02 §5.5: "an
/// unscrollable pane is a defect regardless of content length").
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ScrollState {
    offset: usize,
    total: usize,
    viewport: usize,
}

impl ScrollState {
    /// Records the measured content and viewport, clamping the offset.
    pub(crate) fn measure(&mut self, total: usize, viewport: usize) {
        self.total = total;
        self.viewport = viewport;
        self.offset = self.offset.min(self.max_offset());
    }

    /// Largest legal first-visible line.
    pub(crate) const fn max_offset(&self) -> usize {
        self.total.saturating_sub(self.viewport)
    }

    /// First visible line.
    pub(crate) const fn offset(&self) -> usize {
        self.offset
    }

    pub(crate) fn set_offset(&mut self, offset: usize) {
        self.offset = offset.min(self.max_offset());
    }

    /// Scrolls by `delta` lines, clamped. Returns whether the offset moved.
    pub(crate) fn scroll_by(&mut self, delta: isize) -> bool {
        let before = self.offset;
        let next = if delta < 0 {
            self.offset.saturating_sub(delta.unsigned_abs())
        } else {
            self.offset.saturating_add(delta.unsigned_abs())
        };
        self.offset = next.min(self.max_offset());
        self.offset != before
    }

    /// One viewport up.
    pub(crate) fn page_up(&mut self) -> bool {
        self.scroll_by(-(isize::try_from(self.viewport.max(1)).unwrap_or(1)))
    }

    /// Rows the viewport shows, as last measured.
    pub(crate) const fn viewport(&self) -> usize {
        self.viewport
    }

    /// One viewport down.
    pub(crate) fn page_down(&mut self) -> bool {
        self.scroll_by(isize::try_from(self.viewport.max(1)).unwrap_or(1))
    }

    /// Top of the content.
    pub(crate) fn home(&mut self) -> bool {
        let moved = self.offset != 0;
        self.offset = 0;
        moved
    }

    /// Bottom of the content.
    pub(crate) fn end(&mut self) -> bool {
        let target = self.max_offset();
        let moved = self.offset != target;
        self.offset = target;
        moved
    }

    /// Keeps `line` inside the viewport.
    pub(crate) fn reveal(&mut self, line: usize) {
        if self.viewport == 0 {
            return;
        }
        if line < self.offset {
            self.offset = line;
        } else if line >= self.offset + self.viewport {
            self.offset = line + 1 - self.viewport;
        }
        self.offset = self.offset.min(self.max_offset());
    }
}

/// How much of the original colour survives the scrim, as a sixteenth.
///
/// The transcript still framed every overlay at readable
/// contrast: a quarter-strength blend plus `Modifier::DIM` left the backdrop
/// legible, and `DIM` alone is a no-op on a great many terminals. Two
/// sixteenths puts the backdrop within a hair of the app ground, so the modal
/// is the only thing on screen with contrast.
const SCRIM_KEEP_SIXTEENTHS: u16 = 2;

/// Blends `color` toward `ground`, resolving the colours a cell can carry
/// which are not RGB.
///
/// `Color::Reset` is the terminal's own foreground — full contrast, and the
/// single most common fg on a transcript cell — so it is resolved to `fallback`
/// before blending rather than passed through untouched.
fn dim_toward(color: Color, ground: Color, fallback: Color) -> Color {
    let blend = |a: u8, b: u8| {
        let keep = u16::from(a) * SCRIM_KEEP_SIXTEENTHS;
        let sink = u16::from(b) * (16 - SCRIM_KEEP_SIXTEENTHS);
        u8::try_from((keep + sink) / 16).unwrap_or(b)
    };
    let color = if color == Color::Reset {
        fallback
    } else {
        color
    };
    match (color, ground) {
        (Color::Rgb(r, g, b), Color::Rgb(gr, gg, gb)) => {
            Color::Rgb(blend(r, gr), blend(g, gg), blend(b, gb))
        }
        // An ANSI-16 or indexed colour cannot be blended arithmetically, so
        // the scrim falls back to the ground itself: still an explicit,
        // visible change, never a cell left at full contrast.
        (_, ground) => ground,
    }
}

/// Dims everything already painted in `area` so the modal reads as raised
/// (spec 02 §5.5). Two mechanisms — an RGB blend toward the app ground and the
/// `DIM` modifier — so the effect survives an ANSI16 terminal.
pub(crate) fn render_scrim(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let ground = theme.app_bg;
    // Every scrimmed cell is repainted with an explicit fg and bg taken from
    // the theme. `Modifier::DIM` rides along for the terminals that honour it,
    // but the scrim never depends on it.
    let text = theme.assistant.fg.unwrap_or(ground);
    let buffer = frame.buffer_mut();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            let Some(cell) = buffer.cell_mut((x, y)) else {
                continue;
            };
            let style = cell.style();
            let fg = dim_toward(style.fg.unwrap_or(Color::Reset), ground, text);
            let bg = dim_toward(style.bg.unwrap_or(ground), ground, ground);
            cell.set_style(style.fg(fg).bg(bg).add_modifier(Modifier::DIM));
        }
    }
}

/// Content rows the floating description panel may take, borders excluded.
///
/// One row is the floor — a panel that cannot say one line is not worth the
/// two border rows — and three the ceiling, so the panel never eats the list it
/// describes.
pub(crate) const DESCRIPTION_MIN_ROWS: u16 = 1;
/// See [`DESCRIPTION_MIN_ROWS`].
pub(crate) const DESCRIPTION_MAX_ROWS: u16 = 3;
/// Rows of body the panel gives up when it also carries a value line.
const DESCRIPTION_VALUE_ROWS: u16 = 1;
/// Columns of padding inside the panel border.
const DESCRIPTION_PAD: u16 = 1;

/// The floating description panel for the focused Menu row (§5.5).
///
/// Hovering or focusing a row never reflows the list: the row itself is a
/// highlight and nothing more, and everything the operator needs to read before
/// changing anything is shown here, anchored at the foot of the modal body.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DescriptionPanel {
    /// Focused row's label, drawn on the panel's top border.
    pub title: String,
    /// One-sentence description, word-wrapped inside the panel.
    pub body: String,
    /// `now x · default y` footnote, when the row carries a value.
    pub value: Option<String>,
}

impl DescriptionPanel {
    /// Inner width available to text at `width` columns of panel.
    fn text_width(width: u16) -> usize {
        usize::from(
            width
                .saturating_sub(2)
                .saturating_sub(DESCRIPTION_PAD.saturating_mul(2)),
        )
        .max(1)
    }

    /// Wrapped body rows plus the value row, capped at [`DESCRIPTION_MAX_ROWS`].
    fn lines(&self, width: u16) -> Vec<(String, bool)> {
        self.lines_with_budget(width, DESCRIPTION_MAX_ROWS)
    }

    fn lines_with_budget(&self, width: u16, rows: u16) -> Vec<(String, bool)> {
        let budget = if self.value.is_some() {
            rows.saturating_sub(DESCRIPTION_VALUE_ROWS)
        } else {
            rows
        };
        let mut lines: Vec<(String, bool)> =
            crate::tui::picker::wrap(&self.body, Self::text_width(width))
                .into_iter()
                .take(usize::from(budget))
                .map(|line| (line, false))
                .collect();
        if let Some(value) = &self.value {
            lines.push((
                crate::tui::picker::clip(value, Self::text_width(width)),
                true,
            ));
        }
        lines
    }

    /// Rows the whole panel occupies, borders included.
    pub(crate) fn height(&self, width: u16) -> u16 {
        let rows = u16::try_from(self.lines(width).len())
            .unwrap_or(DESCRIPTION_MAX_ROWS)
            .clamp(DESCRIPTION_MIN_ROWS, DESCRIPTION_MAX_ROWS);
        rows.saturating_add(2)
    }
}

/// A 140-column terminal leaves 136 usable columns after frame and padding.
pub(crate) const WIDE_MENU_COLUMNS: u16 = 136;

/// Keep common list labels and values together (60%, capped at 120), with a
/// two-column gutter and a readable detail column (at most 76 columns).
pub(crate) fn split_wide_detail(body: Rect) -> Option<(Rect, Rect)> {
    if body.width < WIDE_MENU_COLUMNS {
        return None;
    }
    let list_width = body.width.min(200) * 3 / 5;
    let list = Rect {
        width: list_width,
        ..body
    };
    let detail = Rect {
        x: body.x.saturating_add(list_width).saturating_add(2),
        width: body
            .width
            .saturating_sub(list_width)
            .saturating_sub(2)
            .min(76),
        ..body
    };
    Some((list, detail))
}

/// Split a modal body into the rows area and the panel that floats under it.
///
/// The panel is dropped whole rather than squeezing the list below usefulness:
/// a body that cannot spare its rows and still show two rows of list renders no
/// panel at all.
pub(crate) fn split_description(
    body: Rect,
    panel: Option<&DescriptionPanel>,
) -> (Rect, Option<Rect>) {
    let Some(panel) = panel else {
        return (body, None);
    };
    let height = panel.height(body.width);
    if body.width < 8 || body.height < height.saturating_add(2) {
        return (body, None);
    }
    let rows = Rect {
        height: body.height.saturating_sub(height),
        ..body
    };
    let floating = Rect {
        x: body.x,
        y: body.y.saturating_add(rows.height),
        width: body.width,
        height,
    };
    (rows, Some(floating))
}

/// Draw the description panel at `rect`.
pub(crate) fn render_description(
    frame: &mut Frame<'_>,
    rect: Rect,
    theme: &Theme,
    panel: &DescriptionPanel,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(" {} ", panel.title.trim()))
        .title_style(theme.menu_title)
        .border_style(theme.menu_border)
        .style(Style::default().bg(theme.menu_bg));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    for (offset, (line, is_value)) in panel
        .lines_with_budget(rect.width, inner.height)
        .into_iter()
        .enumerate()
    {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        if offset >= inner.height {
            break;
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                line,
                if is_value {
                    theme.slash_item
                } else {
                    theme.slash_item_help
                },
            ))),
            Rect {
                x: inner.x.saturating_add(DESCRIPTION_PAD),
                y: inner.y.saturating_add(offset),
                width: inner.width.saturating_sub(DESCRIPTION_PAD),
                height: 1,
            },
        );
    }
}

/// Chrome inputs for one frame.
pub(crate) struct ModalChrome<'a> {
    /// Border title.
    pub title: &'a str,
    /// Tab labels in declaration order.
    pub tabs: &'a [&'a str],
    /// Index of the active tab.
    pub active: usize,
    /// Footer hints, already phrased as effects.
    pub hints: &'a [Hint],
    /// Whether the pointer is over the close glyph, which then brightens.
    pub close_hover: bool,
    /// Sizing rules for this modal.
    pub sizing: ModalSizing,
    /// When set, and the inner column is wider, the title, tabs, footer and
    /// body share a centered column of this width. The border and close glyph
    /// stay on the full frame. `None` leaves the column full width.
    pub content_max: Option<u16>,
}

/// Centers `content` in a column of at most `max` columns. At or below `max`
/// the rect is unchanged, including its origin.
fn centered_column(content: Rect, max: Option<u16>) -> Rect {
    let Some(max) = max else {
        return content;
    };
    if content.width <= max {
        return content;
    }
    let slack = content.width - max;
    Rect {
        x: content.x.saturating_add(slack / 2),
        width: max,
        ..content
    }
}

/// Draws the scrim, frame, tab bar and footer. Returns `None` below the
/// refuse-to-draw floor, in which case the caller must clear every hit rect.
#[allow(
    clippy::too_many_lines,
    reason = "one frame owns the scrim, border, close glyph, tab bar and footer"
)]
pub(crate) fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &Theme,
    chrome: &ModalChrome<'_>,
) -> Option<ModalLayout> {
    let popup = chrome.sizing.popup(area)?;
    render_scrim(frame, area, theme);
    frame.render_widget(Clear, popup);

    let inner_preview = Block::default().borders(Borders::ALL).inner(popup);
    let full_column = Rect {
        x: inner_preview
            .x
            .saturating_add(chrome.sizing.for_area(area).h_pad),
        y: inner_preview.y,
        width: inner_preview
            .width
            .saturating_sub(chrome.sizing.for_area(area).h_pad.saturating_mul(2)),
        height: inner_preview.height,
    };
    let column = centered_column(full_column, chrome.content_max);
    let title_in_column = column.x != full_column.x || column.width != full_column.width;
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.menu_border)
        .style(Style::default().bg(theme.menu_bg));
    if !title_in_column {
        block = block
            .title(format!(" {} ", chrome.title))
            .title_alignment(ratatui::layout::Alignment::Center)
            .title_style(theme.menu_title);
    }
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if title_in_column {
        let label = format!(" {} ", chrome.title);
        let label_w = u16::try_from(label.width()).unwrap_or(u16::MAX);
        let x = column
            .x
            .saturating_add(column.width.saturating_sub(label_w) / 2);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(label, theme.menu_title))),
            Rect {
                x,
                y: popup.y,
                width: label_w,
                height: 1,
            },
        );
    }

    // `[×]` on the top border, right-aligned two cells in from the corner.
    // U+00D7 sits in the middle of the cell; a latin x rests on the baseline
    // and reads as too low. It brightens under the pointer so the one
    // clickable glyph on the border says so before the click.
    let close = Rect {
        x: popup.x.saturating_add(popup.width.saturating_sub(4)),
        y: popup.y,
        width: 3,
        height: 1,
    };
    let close_style = if chrome.close_hover {
        theme.menu_title.add_modifier(Modifier::BOLD)
    } else {
        theme.chrome
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("[\u{00d7}]", close_style))),
        close,
    );

    if inner.width == 0 || inner.height == 0 {
        return Some(ModalLayout {
            popup,
            close,
            tabs: Vec::new(),
            body: inner,
            footer: inner,
        });
    }

    let sizing = chrome.sizing.for_area(area);
    let content = column;

    let (tab_rows, tabs) = layout_tabs(chrome.tabs, content, inner.height);
    for (index, rect) in &tabs {
        let style = if *index == chrome.active {
            theme.slash_selected
        } else {
            theme.slash_item
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {} ", chrome.tabs[*index]),
                style,
            ))),
            *rect,
        );
    }

    let divider_y = content.y.saturating_add(tab_rows);
    if tab_rows > 0 && divider_y < inner.y.saturating_add(inner.height) {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(usize::from(content.width)),
                theme.menu_border,
            ))),
            Rect {
                x: content.x,
                y: divider_y,
                width: content.width,
                height: 1,
            },
        );
    }

    let footer_rows = u16::try_from(wrap_hints(chrome.hints, content.width).len()).unwrap_or(1);
    let header = tab_rows.saturating_add(u16::from(tab_rows > 0));
    let reserved = header
        .saturating_add(footer_rows)
        .saturating_add(sizing.v_pad);
    let body_height = inner.height.saturating_sub(reserved);
    let body = Rect {
        x: content.x,
        y: content
            .y
            .saturating_add(header)
            .saturating_add(sizing.v_pad),
        width: content.width,
        height: body_height,
    };
    let footer = Rect {
        x: content.x,
        y: inner
            .y
            .saturating_add(inner.height)
            .saturating_sub(footer_rows),
        width: content.width,
        height: footer_rows,
    };
    render_footer(frame, footer, theme, chrome.hints);

    Some(ModalLayout {
        popup,
        close,
        tabs,
        body,
        footer,
    })
}

/// Greedy wrapping tab bar. Tabs that fall past the available rows are omitted
/// from the hit map entirely rather than truncated into an unclickable label.
fn layout_tabs(labels: &[&str], content: Rect, inner_height: u16) -> (u16, Vec<(usize, Rect)>) {
    if labels.is_empty() || content.width == 0 {
        return (0, Vec::new());
    }
    // Never let the tab bar eat the body: cap it at a third of the modal.
    let max_rows = (inner_height / 3).max(1);
    let mut rects = Vec::new();
    let mut row = 0u16;
    let mut x = content.x;
    for (index, label) in labels.iter().enumerate() {
        let width = u16::try_from(label.chars().count() + 2).unwrap_or(u16::MAX);
        if width > content.width {
            continue;
        }
        if x.saturating_add(width) > content.x.saturating_add(content.width) {
            row = row.saturating_add(1);
            x = content.x;
        }
        if row >= max_rows {
            break;
        }
        rects.push((
            index,
            Rect {
                x,
                y: content.y.saturating_add(row),
                width,
                height: 1,
            },
        ));
        x = x.saturating_add(width);
    }
    (row.saturating_add(1), rects)
}

fn render_footer(frame: &mut Frame<'_>, footer: Rect, theme: &Theme, hints: &[Hint]) {
    if footer.width == 0 || footer.height == 0 {
        return;
    }
    for (row, line) in wrap_hints(hints, footer.width).into_iter().enumerate() {
        let Ok(row) = u16::try_from(row) else {
            continue;
        };
        if row >= footer.height {
            break;
        }
        let mut spans = Vec::new();
        for (position, hint) in line.iter().enumerate() {
            if position > 0 {
                spans.push(Span::styled(HINT_SEPARATOR, theme.footer));
            }
            if hint.alert {
                spans.push(Span::styled(
                    format!("{}{HINT_SEPARATOR}{}", hint.effect, hint.key),
                    theme.error,
                ));
                continue;
            }
            spans.push(Span::styled(
                hint.key,
                theme.footer.add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(format!(" {}", hint.effect), theme.footer));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).centered(),
            Rect {
                x: footer.x,
                y: footer.y.saturating_add(row),
                width: footer.width,
                height: 1,
            },
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod tests {
    use super::{Hint, MODAL_MIN_HEIGHT, MODAL_MIN_WIDTH, ModalSizing, ScrollState, wrap_hints};
    use ratatui::layout::Rect;

    fn area(width: u16, height: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    #[test]
    fn wide_detail_breakpoint_and_readable_columns() {
        assert!(super::split_wide_detail(area(135, 30)).is_none());
        let (list, detail) = super::split_wide_detail(area(136, 30)).unwrap();
        assert_eq!(list.width, 81);
        assert_eq!(detail.width, 53);
        assert_eq!(detail.x, list.right() + 2);
        let (list, detail) = super::split_wide_detail(area(216, 30)).unwrap();
        assert_eq!(list.width, 120);
        assert_eq!(detail.width, 76);
        assert_eq!(detail.height, list.height);
    }

    #[test]
    fn refuses_to_draw_below_the_hard_floor() {
        let sizing = ModalSizing::full_page();
        assert!(sizing.popup(area(MODAL_MIN_WIDTH - 1, 40)).is_none());
        assert!(sizing.popup(area(80, MODAL_MIN_HEIGHT - 1)).is_none());
        assert!(
            sizing
                .popup(area(MODAL_MIN_WIDTH, MODAL_MIN_HEIGHT))
                .is_some()
        );
    }

    #[test]
    fn full_page_is_the_whole_terminal_above_the_floor() {
        let sizing = ModalSizing::full_page();
        assert_eq!(sizing.popup(area(80, 24)), Some(area(80, 24)));
        assert_eq!(sizing.popup(area(120, 40)), Some(area(120, 40)));
        assert_eq!(sizing.popup(area(200, 50)), Some(area(200, 50)));
        let shifted = Rect {
            x: 2,
            y: 3,
            width: 120,
            height: 40,
        };
        assert_eq!(sizing.popup(shifted), Some(shifted));
        assert!(sizing.full_screen);
        assert_eq!((sizing.h_pad, sizing.v_pad, sizing.v_margin), (1, 0, 0));
    }

    #[test]
    fn compact_and_narrow_rules_drop_margin_and_padding() {
        let sizing = ModalSizing::resume_picker();
        let compact = sizing.for_area(area(120, 24));
        assert_eq!((compact.v_margin, compact.v_pad, compact.h_pad), (0, 0, 1));
        let narrow = sizing.for_area(area(50, 40));
        assert_eq!((narrow.width_num, narrow.width_den), (1, 1));
        assert_eq!(narrow.h_pad, 1);
        let roomy = sizing.for_area(area(200, 50));
        assert_eq!((roomy.v_margin, roomy.v_pad, roomy.h_pad), (4, 1, 2));
    }

    #[test]
    fn popup_is_centered_and_clamped() {
        let sizing = ModalSizing::resume_picker();
        let popup = sizing.popup(area(200, 50)).expect("roomy terminal draws");
        assert_eq!(popup.width, 120, "max width clamps the fraction");
        assert_eq!(popup.x, (200 - 120) / 2);
        assert_eq!(popup.height, 50 - 8);
        assert_eq!(popup.y, 4);
        let popup = sizing.popup(area(80, 24)).expect("80x24 draws");
        assert_eq!(popup.height, 24, "compact drops the vertical margin");
        assert_eq!(popup.width, 52);
    }

    #[test]
    fn hints_wrap_greedily_into_rows() {
        let hints = [
            Hint::new("Space", "disable"),
            Hint::new("Enter", "select"),
            Hint::new("Esc", "close"),
        ];
        assert_eq!(wrap_hints(&hints, 200).len(), 1);
        let rows = wrap_hints(&hints, 20);
        assert!(
            rows.len() > 1,
            "a narrow footer wraps rather than truncates"
        );
        assert_eq!(
            rows.iter().map(Vec::len).sum::<usize>(),
            hints.len(),
            "wrapping never drops a hint"
        );
    }

    #[test]
    fn scroll_state_pages_clamps_and_reveals() {
        let mut scroll = ScrollState::default();
        scroll.measure(100, 10);
        assert!(scroll.page_down());
        assert_eq!(scroll.offset(), 10);
        assert!(scroll.end());
        assert_eq!(scroll.offset(), 90);
        assert!(!scroll.page_down(), "clamped at the bottom");
        assert!(scroll.home());
        assert_eq!(scroll.offset(), 0);
        scroll.reveal(42);
        assert_eq!(scroll.offset(), 33);
        scroll.reveal(0);
        assert_eq!(scroll.offset(), 0);
        scroll.measure(5, 10);
        assert_eq!(scroll.max_offset(), 0);
        assert!(!scroll.page_down(), "short content does not scroll");
    }
}
