//! Transcript copy drops code and quote rails and copies a clipped code row
//! whole (spec 02 §5.4).

use super::App;
use super::test_support::{left_mouse, say, transcript_fixture};
use crate::tui::HostEvent;
use crossterm::event::{MouseButton, MouseEventKind};
use plakous_host_protocol::views::AgentTranscript;
use plakous_host_protocol::views::transcript::TranscriptRow;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use unicode_width::UnicodeWidthStr as _;

const SHELL: &str = "plak logout claude --local\n\n    plak logout codex --local";

fn fenced(lang: &str, body: &str) -> String {
    if lang.is_empty() {
        format!("```\n{body}\n```")
    } else {
        format!("```{lang}\n{body}\n```")
    }
}

fn paint(app: &mut App, width: u16, height: u16) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
}

fn plain(app: &App) -> Vec<String> {
    app.pane.transcript_cache.plain().to_vec()
}

fn rows_containing(rows: &[String], needle: &str) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| row.contains(needle))
        .map(|(index, _)| index)
        .collect()
}

fn cell_at(line: &str, needle: &str) -> usize {
    let byte = line
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} not in {line}"));
    line[..byte].width()
}

/// Drag across rendered cells. The gesture must move at least two cells or
/// mouse-up treats it as a click and clears the selection.
fn select_cells(app: &mut App, row: usize, first: usize, last_row: usize, last: usize) {
    let body = app.transcript_body_area.expect("transcript body");
    let start = app.pane.transcript_visible_start;
    assert!(
        row >= start && last_row >= start && last_row < start + usize::from(body.height),
        "selection scrolled off: start {start} rows {row}..={last_row} body {body:?}"
    );
    let y0 = body.y + u16::try_from(row - start).unwrap();
    let y1 = body.y + u16::try_from(last_row - start).unwrap();
    let x0 = body.x + u16::try_from(first).unwrap();
    let x1 = body.x + u16::try_from(last).unwrap();
    assert!(
        x0 < body.right() && x1 < body.right() && y1 < body.bottom(),
        "selection outside {body:?}: ({x0},{y0})-({x1},{y1})"
    );
    assert!(
        x0.abs_diff(x1).max(y0.abs_diff(y1)) >= 2,
        "a shorter drag is a click"
    );
    app.handle_mouse(left_mouse(MouseEventKind::Down(MouseButton::Left), x0, y0));
    app.handle_mouse(left_mouse(MouseEventKind::Drag(MouseButton::Left), x1, y1));
    app.handle_mouse(left_mouse(MouseEventKind::Up(MouseButton::Left), x1, y1));
}

fn copy_rows(app: &mut App, width: u16, height: u16, first: usize, last: usize) -> String {
    let end = app
        .transcript_body_area
        .expect("body")
        .width
        .saturating_sub(1);
    select_cells(app, first, 0, last, usize::from(end));
    paint(app, width, height);
    app.take_transcript_selection_text()
        .unwrap_or_else(|| panic!("nothing copied from rows {first}..={last}"))
}

fn sample() -> String {
    format!(
        "Ready.\n\n> a quoted sentence\n\n- alpha item\n- beta item\n\n| Path | Role |\n| --- | --- |\n| plakous/ | facade |\n\n{}\n\n{}",
        fenced("bash", SHELL),
        fenced("", SHELL),
    )
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one selection covers the fence, the quote, the list and the table"
)]
fn copying_a_fenced_block_drops_rails_and_keeps_lists_quotes_and_tables() {
    const WIDTH: u16 = 100;
    const HEIGHT: u16 = 40;
    let mut app = transcript_fixture();
    say(&mut app, &sample());
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let claude = rows_containing(&rows, "plak logout claude");
    let codex = rows_containing(&rows, "plak logout codex");
    assert_eq!(claude.len(), 2, "{rows:?}");
    assert_eq!(codex.len(), 2, "{rows:?}");

    let bash_lang = claude[0] - 1;
    assert!(
        rows[bash_lang].contains("bash") && rows[bash_lang].contains('┌'),
        "language label row: {:?}",
        rows[bash_lang]
    );
    assert!(
        rows[claude[0]].starts_with("  │ plak logout claude"),
        "{:?}",
        rows[claude[0]]
    );
    assert!(
        rows[codex[0]].starts_with("  │     plak logout codex"),
        "{:?}",
        rows[codex[0]]
    );
    let layout = app.pane.transcript_cache.layout();
    assert!(layout.copy[bash_lang].language);
    assert!(layout.copy[bash_lang].code.is_none());
    assert_eq!(
        layout.copy[claude[0]].code.as_deref(),
        Some("plak logout claude --local")
    );
    assert!(layout.copy[claude[0]].decoration > 2);

    let bash = copy_rows(&mut app, WIDTH, HEIGHT, bash_lang, codex[0]);
    assert_eq!(bash, SHELL, "labelled fence included decoration: {bash:?}");
    assert!(!bash.contains("bash") && !bash.contains('│') && !bash.contains('┌'));

    paint(&mut app, WIDTH, HEIGHT);
    let bare = copy_rows(&mut app, WIDTH, HEIGHT, claude[1], codex[1]);
    assert_eq!(bare, SHELL, "bare fence: {bare:?}");

    // A selection inside one line copies those cells and does not expand.
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let at = cell_at(&rows[claude[0]], "logout");
    select_cells(&mut app, claude[0], at, claude[0], at + "logout".len() - 1);
    paint(&mut app, WIDTH, HEIGHT);
    assert_eq!(
        app.take_transcript_selection_text().as_deref(),
        Some("logout")
    );

    // Inside the language label: not the whole row, so the cells stay.
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let label = cell_at(&rows[bash_lang], "bash");
    select_cells(
        &mut app,
        bash_lang,
        label + 1,
        bash_lang,
        label + "ash".len(),
    );
    paint(&mut app, WIDTH, HEIGHT);
    assert_eq!(app.take_transcript_selection_text().as_deref(), Some("ash"));

    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let quote = rows_containing(&rows, "quoted sentence")[0];
    let item = rows_containing(&rows, "alpha item")[0];
    let table = rows_containing(&rows, "plakous/")[0];
    let prose = rows_containing(&rows, "Ready.")[0];
    assert!(rows[quote].contains('│'), "{:?}", rows[quote]);
    assert!(rows[item].contains('•'), "{:?}", rows[item]);
    assert!(rows[table].contains('│'), "{:?}", rows[table]);
    let quote_copy = copy_rows(&mut app, WIDTH, HEIGHT, quote, quote);
    assert_eq!(quote_copy, "a quoted sentence", "{quote_copy:?}");
    paint(&mut app, WIDTH, HEIGHT);
    let item_copy = copy_rows(&mut app, WIDTH, HEIGHT, item, item);
    assert!(
        item_copy.starts_with("• ") && item_copy.contains("alpha item"),
        "list marker dropped: {item_copy:?}"
    );
    paint(&mut app, WIDTH, HEIGHT);
    let table_copy = copy_rows(&mut app, WIDTH, HEIGHT, table, table);
    assert!(
        table_copy.contains('│') && table_copy.contains("plakous/"),
        "table rail dropped: {table_copy:?}"
    );
    assert!(
        !table_copy.starts_with("  │"),
        "block indent stayed: {table_copy:?}"
    );
    paint(&mut app, WIDTH, HEIGHT);
    let prose_copy = copy_rows(&mut app, WIDTH, HEIGHT, prose, prose);
    assert_eq!(prose_copy, "Ready.", "block indent stayed: {prose_copy:?}");
}

#[test]
fn copying_a_clipped_code_row_through_its_end_copies_the_source_line() {
    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 30;
    let long = "plak logout claude --local --and-then-remove-the-stored-token-cache";
    let mut app = transcript_fixture();
    say(&mut app, &fenced("", &format!("{long}\necho done")));
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let wide = rows_containing(&rows, "plak logout claude");
    let short = rows_containing(&rows, "echo done");
    assert_eq!(wide.len(), 1, "{rows:?}");
    assert_eq!(short.len(), 1, "{rows:?}");
    assert!(
        !rows[wide[0]].contains("token-cache"),
        "the row should be clipped at this width: {:?}",
        rows[wide[0]]
    );
    assert!(rows[wide[0]].contains('│'), "{:?}", rows[wide[0]]);

    let copied = copy_rows(&mut app, WIDTH, HEIGHT, wide[0], short[0]);
    assert_eq!(copied, format!("{long}\necho done"), "{copied:?}");

    // Starting inside the line keeps the painted cells and not the hidden tail.
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let at = cell_at(&rows[wide[0]], "logout");
    select_cells(&mut app, wide[0], at, wide[0], at + "logout".len() - 1);
    paint(&mut app, WIDTH, HEIGHT);
    let partial = app.take_transcript_selection_text().unwrap();
    assert_eq!(partial, "logout");
    assert!(!partial.contains("token-cache"));
}

#[test]
fn agent_transcript_copy_drops_the_same_code_rails() {
    const WIDTH: u16 = 100;
    const HEIGHT: u16 = 40;
    let mut app = transcript_fixture();
    app.open_agent_view("agent-c3".into(), "#c3 · reviewer".into());
    app.apply_host_event(HostEvent::AgentTranscript(AgentTranscript {
        agent_id: "agent-c3".into(),
        rows: vec![TranscriptRow::Assistant {
            text: fenced("bash", SHELL),
        }],
        live_turn_id: None,
        note: None,
    }));
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let claude = rows_containing(&rows, "plak logout claude");
    let codex = rows_containing(&rows, "plak logout codex");
    assert_eq!(claude.len(), 1, "{rows:?}");
    assert_eq!(codex.len(), 1, "{rows:?}");
    let lang = claude[0] - 1;
    assert!(rows[lang].contains('┌'), "{:?}", rows[lang]);
    assert!(
        rows[claude[0]].contains('│'),
        "agent view did not paint the code rail: {:?}",
        rows[claude[0]]
    );
    let copied = copy_rows(&mut app, WIDTH, HEIGHT, lang, codex[0]);
    assert_eq!(copied, SHELL, "{copied:?}");
}

/// A Makefile recipe and a `<<-EOF` heredoc paste with their tabs. The painted
/// row may show those tabs as spaces.
#[test]
fn copying_a_makefile_keeps_tabs() {
    const WIDTH: u16 = 100;
    const HEIGHT: u16 = 40;
    let src = "all:\n\techo hi\n\tcat <<-EOF\n\t\tbody\n\tEOF";
    let mut app = transcript_fixture();
    say(&mut app, &fenced("make", src));
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let all = rows_containing(&rows, "all:");
    let eof = rows_containing(&rows, "EOF");
    assert_eq!(all.len(), 1, "{rows:?}");
    assert!(eof.len() >= 2, "heredoc opener and closer: {rows:?}");
    let echo = all[0] + 1;
    assert!(
        rows[echo].contains("echo hi") && !rows[echo].contains('\t'),
        "painted cells keep spaces: {:?}",
        rows[echo]
    );
    let copied = copy_rows(&mut app, WIDTH, HEIGHT, all[0], *eof.last().expect("EOF"));
    assert_eq!(copied, src, "rows {rows:#?}");
}
