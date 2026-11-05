//! Transcript copy drops code and quote rails and copies a wrapped code row
//! whole (spec 02 §5.4). The selection highlight paints those same cells.

use super::App;
use super::test_support::{left_mouse, say, tool_line, transcript_fixture};
use crate::config::user_action_channel;
use crate::tui::HostEvent;
use crate::tui::presentation::ActivityKind;
use crossterm::event::{MouseButton, MouseEventKind};
use plakous_host_protocol::views::transcript::{AgentMilestone, MilestoneEvent, TranscriptRow};
use plakous_host_protocol::views::{CallDetail, CallDetailOutcome, CallDetailSource};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use unicode_width::UnicodeWidthStr as _;

const SHELL: &str = "plak logout claude\n\n    plak logout codex";

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

fn row_highlight(app: &App, buffer: &ratatui::buffer::Buffer, row: usize) -> String {
    let body = app.transcript_body_area.expect("body");
    let y = body.y + u16::try_from(row - app.pane.transcript_visible_start).unwrap();
    let style = app.theme.transcript_selection;
    let mut text = String::new();
    for x in body.x..body.right() {
        let cell = &buffer[(x, y)];
        if Some(cell.fg) == style.fg && Some(cell.bg) == style.bg {
            text.push_str(cell.symbol());
        }
    }
    text
}

fn highlighted_equals_copy(
    app: &mut App,
    width: u16,
    height: u16,
    row: usize,
    first: usize,
    last: usize,
) -> String {
    select_cells(app, row, first, row, last);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let highlighted = row_highlight(app, terminal.backend().buffer(), row);
    let copied = app.take_transcript_selection_text().unwrap_or_default();
    assert_eq!(highlighted, copied, "plain {:?}", plain(app).get(row));
    highlighted
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
        Some("plak logout claude")
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
fn copying_a_wrapped_code_row_through_its_end_copies_the_source_line() {
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
        "the row should be wrapped at this width: {:?}",
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
    app.apply_host_event(super::test_support::agent_snapshot(
        "agent-c3",
        1,
        vec![TranscriptRow::Assistant {
            text: fenced("bash", SHELL),
        }],
    ));
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

#[test]
fn selection_highlight_matches_copied_transcript_cells() {
    const WIDTH: u16 = 100;
    const HEIGHT: u16 = 40;
    let mut app = transcript_fixture();
    say(&mut app, &sample());
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let edge = usize::from(app.transcript_body_area.unwrap().width) - 1;
    let prose = rows_containing(&rows, "Ready.")[0];
    let quote = rows_containing(&rows, "quoted sentence")[0];
    let item = rows_containing(&rows, "alpha item")[0];
    let table = rows_containing(&rows, "plakous/")[0];
    let code = rows_containing(&rows, "plak logout claude")[0];
    let lang = code - 1;

    assert_eq!(
        highlighted_equals_copy(&mut app, WIDTH, HEIGHT, prose, 0, edge),
        "Ready."
    );
    assert_eq!(
        highlighted_equals_copy(&mut app, WIDTH, HEIGHT, quote, 0, edge),
        "a quoted sentence"
    );
    let listed = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, item, 0, edge);
    assert!(
        listed.starts_with("• ") && listed.contains("alpha item"),
        "{listed:?}"
    );
    let tabled = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, table, 0, edge);
    assert!(
        tabled.starts_with('│') && tabled.contains("plakous/"),
        "{tabled:?}"
    );
    assert_eq!(
        highlighted_equals_copy(&mut app, WIDTH, HEIGHT, code, 0, edge),
        "plak logout claude"
    );
    assert_eq!(
        highlighted_equals_copy(&mut app, WIDTH, HEIGHT, lang, 0, edge),
        ""
    );

    let rows = plain(&app);
    let at = cell_at(&rows[code], "logout");
    let partial = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, code, 1, at + 5);
    assert!(
        partial.ends_with("logout") && !partial.contains('│'),
        "{partial:?}"
    );
    assert_eq!(
        highlighted_equals_copy(&mut app, WIDTH, HEIGHT, code, 0, 2),
        ""
    );
}

#[test]
fn wrapped_code_highlight_matches_the_copied_cells() {
    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 30;
    let long = "plak logout claude --local --and-then-remove-the-stored-token-cache";
    let mut app = transcript_fixture();
    say(&mut app, &fenced("", &format!("{long}\necho done")));
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let wide = rows_containing(&rows, "plak logout claude")[0];
    let cont = rows
        .iter()
        .position(|row| row.contains('↳'))
        .expect("continuation");
    let marker = cell_at(&rows[cont], "↳");
    select_cells(&mut app, wide, 0, cont, marker + 4);
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let (head, tail) = {
        let buffer = terminal.backend().buffer();
        (
            row_highlight(&app, buffer, wide),
            row_highlight(&app, buffer, cont),
        )
    };
    let copied = app.take_transcript_selection_text().unwrap_or_default();
    assert_eq!(format!("{head}{tail}"), copied);
    assert!(
        !head.contains('│') && !tail.contains('↳'),
        "{head:?} {tail:?}"
    );
}

#[test]
fn prompt_highlight_omits_the_send_time_and_steer_tag() {
    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 24;
    let sent = 1_791_383_520_000;
    let mut app = transcript_fixture();
    app.apply_host_event(HostEvent::Row(TranscriptRow::User {
        text: "ship it".into(),
        sent_at_ms: Some(sent),
    }));
    app.apply_host_event(HostEvent::Row(TranscriptRow::Steer {
        text: "also docs".into(),
        sent_at_ms: Some(sent),
    }));
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let edge = usize::from(app.transcript_body_area.unwrap().width) - 1;
    let prompt = rows_containing(&rows, "ship it")[0];
    let steer = rows_containing(&rows, "also docs")[0];
    let prompt_text = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, prompt, 0, edge);
    assert!(prompt_text.contains("ship it"), "{prompt_text:?}");
    assert!(!prompt_text.contains("steer"), "{prompt_text:?}");
    let steer_text = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, steer, 0, edge);
    assert!(
        steer_text.contains("also docs") && !steer_text.contains("steer"),
        "{steer_text:?}"
    );
}

#[test]
fn cell_output_gutter_highlight_matches_copy() {
    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 40;
    let mut app = transcript_fixture();
    app.render_prefs.reduced_motion = true;
    super::test_support::compose_cell(
        &mut app,
        "cell",
        "exec_js",
        "done",
        "0 tools",
        "const x = 1;",
        &[],
    );
    say(&mut app, "Done.");
    app.toggle_tool_expand_at(0);
    paint(&mut app, WIDTH, HEIGHT);
    let output = rows_containing(&plain(&app), "output ▸")[0];
    let edge = usize::from(app.transcript_body_area.unwrap().width) - 1;
    assert_eq!(
        highlighted_equals_copy(&mut app, WIDTH, HEIGHT, output, 0, edge),
        "output ▸"
    );
}

#[test]
fn opened_cell_output_highlight_drops_the_rail() {
    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 40;
    let mut app = transcript_fixture();
    app.render_prefs.reduced_motion = true;
    super::test_support::compose_cell(
        &mut app,
        "cell",
        "exec_js",
        "done",
        "0 tools",
        "const x = 1;",
        &[],
    );
    say(&mut app, "Done.");
    app.toggle_tool_expand_at(0);
    app.apply_host_event(canonical_detail("cell", "{}", "alpha\nbeta"));
    app.toggle_row(0, crate::tui::blocks::RowTarget::CellOutput(0));
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let edge = usize::from(app.transcript_body_area.unwrap().width) - 1;
    let alpha = rows_containing(&rows, "alpha")[0];
    let beta = rows_containing(&rows, "beta")[0];
    assert!(rows[alpha].contains('│'), "output rail: {:?}", rows[alpha]);
    assert_eq!(
        highlighted_equals_copy(&mut app, WIDTH, HEIGHT, alpha, 0, edge),
        "alpha"
    );
    let from_rail = highlighted_equals_copy(
        &mut app,
        WIDTH,
        HEIGHT,
        alpha,
        0,
        cell_at(&rows[alpha], "alpha") + 2,
    );
    assert_eq!(from_rail, "alp");
    assert_no_rail(&from_rail, &rows[alpha]);
    let copied = copy_rows(&mut app, WIDTH, HEIGHT, alpha, beta);
    assert_eq!(copied, "alpha\nbeta");
    assert_no_rail(&copied, &rows[beta]);
}

fn canonical_detail(call_id: &str, input: &str, output: &str) -> HostEvent {
    HostEvent::CallDetail(CallDetail {
        call_id: call_id.to_owned(),
        agent_id: None,
        session_id: "s1".to_owned(),
        outcome: CallDetailOutcome::Detail {
            input: input.to_owned(),
            output: output.to_owned(),
            input_truncated: false,
            output_truncated: false,
            source: CallDetailSource::Canonical,
            sources: Vec::new(),
        },
    })
}

fn open_item(app: &mut App, width: u16, height: u16) {
    app.render_prefs.reduced_motion = true;
    paint(app, width, height);
    app.toggle_tool_expand_at(0);
    let (tx, _rx) = user_action_channel();
    app.maybe_request_call_details(Some(&tx));
}

fn assert_no_rail(copied: &str, plain_row: &str) {
    assert!(
        !copied.contains('│') && !copied.contains('├') && !copied.contains('└'),
        "rail copied {copied:?} from {plain_row:?}"
    );
}

#[test]
fn tool_detail_highlight_drops_the_rail_whole_and_partial() {
    const WIDTH: u16 = 100;
    const HEIGHT: u16 = 40;
    let mut app = transcript_fixture();
    app.apply_host_event(tool_line(
        "run",
        None,
        None,
        ActivityKind::Process,
        "exec_command",
        "done",
        Some("cargo test --workspace"),
    ));
    say(&mut app, "Done.");
    open_item(&mut app, WIDTH, HEIGHT);
    app.apply_host_event(canonical_detail(
        "run",
        r#"{"cmd":"cargo test --workspace"}"#,
        "applied",
    ));
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let edge = usize::from(app.transcript_body_area.unwrap().width) - 1;
    let command = rows
        .iter()
        .position(|row| row.contains("│ $ cargo test --workspace"))
        .expect("detail row");
    let line = rows[command].clone();
    assert!(line.contains('│'), "detail rail not painted: {line:?}");
    let dollar = cell_at(&line, "$");
    assert!(dollar >= 2, "{line:?}");

    let whole = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, command, 0, edge);
    assert_eq!(whole, "$ cargo test --workspace");
    assert_no_rail(&whole, &line);

    let from_rail = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, command, 0, dollar + 5);
    assert_eq!(from_rail, "$ carg");
    assert_no_rail(&from_rail, &line);

    assert_eq!(
        highlighted_equals_copy(&mut app, WIDTH, HEIGHT, command, 0, dollar - 1),
        ""
    );

    let cargo = cell_at(&line, "cargo");
    assert_eq!(
        highlighted_equals_copy(&mut app, WIDTH, HEIGHT, command, cargo, cargo + 4),
        "cargo"
    );

    let output = plain(&app)
        .iter()
        .position(|row| row.contains("│ applied"))
        .expect("output row");
    let output_line = plain(&app)[output].clone();
    let applied = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, output, 0, edge);
    assert_eq!(applied, "applied");
    assert_no_rail(&applied, &output_line);
}

#[test]
fn diff_highlight_drops_the_rail_and_keeps_markers() {
    const WIDTH: u16 = 100;
    const HEIGHT: u16 = 40;
    let mut app = transcript_fixture();
    app.apply_host_event(tool_line(
        "edit",
        None,
        None,
        ActivityKind::Tool,
        "edit_file",
        "done",
        Some("src/a.rs +1/-1"),
    ));
    say(&mut app, "x");
    open_item(&mut app, WIDTH, HEIGHT);
    app.apply_host_event(canonical_detail(
        "edit",
        r#"{"file_path":"src/a.rs","old_string":"one\ntwo\nthree","new_string":"one\n2\nthree"}"#,
        "applied",
    ));
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let edge = usize::from(app.transcript_body_area.unwrap().width) - 1;
    let removed = rows_containing(&rows, "-two")[0];
    let line = rows[removed].clone();
    assert!(line.contains('│'), "diff rail not painted: {line:?}");
    let marker = cell_at(&line, "-two");

    let whole = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, removed, 0, edge);
    assert_eq!(whole, "-two");
    assert_no_rail(&whole, &line);

    let from_rail = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, removed, 0, marker + 1);
    assert_eq!(from_rail, "-t");
    assert_no_rail(&from_rail, &line);

    assert_eq!(
        highlighted_equals_copy(&mut app, WIDTH, HEIGHT, removed, marker, marker + 3),
        "-two"
    );

    let context = rows_containing(&rows, " one")[0];
    let context_line = rows[context].clone();
    let kept = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, context, 0, edge);
    assert_eq!(kept, " one");
    assert_no_rail(&kept, &context_line);
}

#[test]
fn wrapped_detail_highlight_drops_the_hanging_indent() {
    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 40;
    let long = "alphaBRAVOcharlieDELTAechoFOXTROTgolfHOTELextraTAIL";
    let mut app = transcript_fixture();
    app.apply_host_event(tool_line(
        "run",
        None,
        None,
        ActivityKind::Process,
        "exec_command",
        "done",
        Some("cargo test"),
    ));
    say(&mut app, "Done.");
    open_item(&mut app, WIDTH, HEIGHT);
    app.apply_host_event(canonical_detail("run", r#"{"cmd":"cargo test"}"#, long));
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let edge = usize::from(app.transcript_body_area.unwrap().width) - 1;
    let start = rows_containing(&rows, "alphaBRAVO")[0];
    let end = rows_containing(&rows, "extraTAIL")[0];
    assert!(end > start, "the output did not wrap: {rows:?}");
    let mut joined = String::new();
    for (offset, plain_row) in rows[start..=end].iter().enumerate() {
        let row = start + offset;
        let copied = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, row, 0, edge);
        assert_no_rail(&copied, plain_row);
        if row > start {
            assert!(
                !copied.starts_with(' '),
                "hanging indent copied {copied:?} from {plain_row:?}"
            );
        }
        joined.push_str(&copied);
    }
    assert_eq!(joined, long, "{rows:?}");
}

#[test]
fn thinking_detail_highlight_drops_the_rail_whole_and_partial() {
    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 40;
    let long = "alphaBRAVOcharlieDELTAechoFOXTROTgolfHOTELextraTAIL";
    let mut app = transcript_fixture();
    app.render_prefs.reduced_motion = true;
    app.apply_host_event(HostEvent::ReasoningDelta {
        location_key: "r1".into(),
        text: format!("line one\n{long}"),
    });
    app.apply_host_event(HostEvent::ReasoningSealed {
        location_key: "r1".into(),
    });
    say(&mut app, "Done.");
    let idx = app
        .pane
        .blocks
        .iter()
        .position(|block| matches!(block, crate::tui::blocks::ScrollBlock::Thinking { .. }))
        .expect("thought");
    app.toggle_tool_expand_at(idx);
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let edge = usize::from(app.transcript_body_area.unwrap().width) - 1;
    let head = rows
        .iter()
        .position(|row| row.contains("line one") && !row.contains('◆'))
        .expect("detail");
    let head_line = rows[head].clone();
    assert!(
        head_line.contains('│') || head_line.starts_with("    "),
        "thought rail not painted: {head_line:?}"
    );
    let whole = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, head, 0, edge);
    assert_eq!(whole, "line one");
    assert_no_rail(&whole, &head_line);

    let word = cell_at(&head_line, "line");
    assert!(word >= 2, "{head_line:?}");
    let partial = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, head, 0, word + 3);
    assert_eq!(partial, "line");
    assert_no_rail(&partial, &head_line);
    assert_eq!(
        highlighted_equals_copy(&mut app, WIDTH, HEIGHT, head, 0, word - 1),
        ""
    );

    let start = rows_containing(&rows, "alphaBRAVO")[0];
    let end = rows_containing(&rows, "extraTAIL")[0];
    assert!(end >= start, "body missing: {rows:?}");
    let mut joined = String::new();
    for (offset, plain_row) in rows[start..=end].iter().enumerate() {
        let row = start + offset;
        let copied = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, row, 0, edge);
        assert_no_rail(&copied, plain_row);
        if row > start {
            assert!(
                !copied.starts_with(' '),
                "hanging indent copied {copied:?} from {plain_row:?}"
            );
        }
        joined.push_str(&copied);
    }
    assert_eq!(joined, long, "{rows:?}");
}

#[test]
fn milestone_detail_highlight_drops_the_rail_whole_and_partial() {
    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 40;
    let long = "alphaBRAVOcharlieDELTAechoFOXTROTgolfHOTELextraTAIL";
    let mut app = transcript_fixture();
    app.render_prefs.reduced_motion = true;
    app.apply_host_event(HostEvent::Row(TranscriptRow::Milestone(AgentMilestone {
        agent_id: "c23".into(),
        event: MilestoneEvent::Spawned,
        template: Some("worker".into()),
        text: Some(format!("Port the renderer\n{long}")),
        duration_ms: None,
        candidate: None,
        files: None,
        paths: Vec::new(),
    })));
    say(&mut app, "Done.");
    let idx = app
        .pane
        .blocks
        .iter()
        .position(|block| matches!(block, crate::tui::blocks::ScrollBlock::Milestone { .. }))
        .expect("milestone");
    app.toggle_tool_expand_at(idx);
    paint(&mut app, WIDTH, HEIGHT);
    let rows = plain(&app);
    let edge = usize::from(app.transcript_body_area.unwrap().width) - 1;
    let head = rows
        .iter()
        .position(|row| row.contains('│') && row.contains("Port the renderer"))
        .expect("detail");
    let head_line = rows[head].clone();
    assert!(
        head_line.contains('│'),
        "milestone rail not painted: {head_line:?}"
    );
    let whole = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, head, 0, edge);
    assert_eq!(whole, "Port the renderer");
    assert_no_rail(&whole, &head_line);

    let word = cell_at(&head_line, "Port");
    assert!(word >= 2, "{head_line:?}");
    let partial = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, head, 0, word + 3);
    assert_eq!(partial, "Port");
    assert_no_rail(&partial, &head_line);
    assert_eq!(
        highlighted_equals_copy(&mut app, WIDTH, HEIGHT, head, word, word + 3),
        "Port"
    );

    let start = rows_containing(&rows, "alphaBRAVO")[0];
    let end = rows_containing(&rows, "extraTAIL")[0];
    assert!(end >= start, "body missing: {rows:?}");
    let mut joined = String::new();
    for (offset, plain_row) in rows[start..=end].iter().enumerate() {
        let row = start + offset;
        let copied = highlighted_equals_copy(&mut app, WIDTH, HEIGHT, row, 0, edge);
        assert_no_rail(&copied, plain_row);
        if row > start {
            assert!(
                !copied.starts_with(' '),
                "hanging indent copied {copied:?} from {plain_row:?}"
            );
        }
        joined.push_str(&copied);
    }
    assert_eq!(joined, long, "{rows:?}");
}
