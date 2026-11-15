//! Live TUI rows against the rows a resume rebuilds from the feed (spec 02
//! §2.6): the semantic pilot reads the live transcript tail of a finished
//! turn, the product views are deleted, and a resumed TUI over the rebuilt
//! views shows the same tail, while the rebuilt views equal the live ones.

use super::*;
use plak::home::paths::PlakPaths;
use plak::store::session_records::{SessionRecordStore, SqliteSessionStore};

use super::projection_parity::{ANSWER, script};

/// Launches the TUI under a fresh pilot directory and waits for its bound,
/// idle composer.
fn launch(
    fixture: &Fixture,
    pilot_dir: &str,
    resume: Option<&str>,
) -> (TerminalChild, PilotClient) {
    let pilot = PilotClient::new(fixture.root.join(pilot_dir));
    let mut terminal = TerminalChild::spawn_with_pilot(fixture, 100, resume, Some(&pilot.root));
    terminal.wait_screen("? keybinds");
    let mut pilot = pilot;
    pilot.observe_until("the bound, idle agent", |document| {
        document["snapshot"]["binding"]["attached"] == true
            && document["snapshot"]["phase"] == "idle"
    });
    (terminal, pilot)
}

fn tail(document: &Value) -> Vec<String> {
    document["snapshot"]["transcript"]["tail"]
        .as_array()
        .expect("transcript tail")
        .iter()
        .map(|line| line.as_str().expect("tail line").to_owned())
        .collect()
}

fn quit(terminal: TerminalChild, pilot: &mut PilotClient) {
    let quit = pilot.act("quit", None);
    assert_eq!(quit["error"], Value::Null);
    assert!(terminal.finish().success());
}

#[test]
fn live_tui_rows_equal_the_rows_a_resume_rebuilds_from_deleted_views() {
    for shape in [
        "final_only",
        "direct_tool_then_final",
        "hosted_tool_call_final",
        "code_mode_cell_nested_then_final",
        "manual_compaction",
        "in_turn_compaction",
    ] {
        let fixture = Fixture::new(&script(shape));
        let (mut terminal, mut pilot) = launch(&fixture, "pilot-live", None);
        pilot.observe();
        if matches!(shape, "manual_compaction" | "in_turn_compaction") {
            pilot.act("type-text", Some("seed a foldable turn"));
            assert_eq!(pilot.act("send", None)["error"], Value::Null);
            pilot.observe_until("the seed going idle", |document| {
                document["snapshot"]["phase"] == "idle"
                    && document["snapshot"]["turn"]["phase"] == "completed"
            });
        }
        pilot.act("type-text", Some("pilot parity prompt"));
        let sent = pilot.act("send", None);
        assert_eq!(sent["error"], Value::Null, "{shape}");
        terminal.wait_screen(ANSWER);
        let mut finished = pilot.observe_until("the turn going idle", |document| {
            document["snapshot"]["phase"] == "idle"
                && document["snapshot"]["turn"]["phase"] == "completed"
        });
        if shape == "manual_compaction" {
            pilot.act("type-text", Some("/compact"));
            assert_eq!(pilot.act("accept", None)["error"], Value::Null);
            finished = pilot.observe_until("the compact row settling", |document| {
                document["snapshot"]["phase"] == "idle"
                    && document["snapshot"]["busy"] == false
                    && tail(document)
                        .iter()
                        .any(|line| line.to_ascii_lowercase().contains("compact"))
            });
        }
        if matches!(shape, "manual_compaction" | "in_turn_compaction") {
            assert_eq!(
                tail(&finished)
                    .iter()
                    .filter(|line| line.to_ascii_lowercase().contains("compact"))
                    .count(),
                1,
                "one compact row in the live TUI: {finished}"
            );
        }
        let session = finished["snapshot"]["binding"]["session_id"]
            .as_str()
            .expect("bound session id")
            .to_owned();
        let live_tail = tail(&finished);
        quit(terminal, &mut pilot);

        let paths = PlakPaths::explicit(fixture.home());
        let store = SqliteSessionStore::new(paths.clone());
        let live = store.load_views(&session).unwrap().expect("live views");
        let mut product = plak::store::ProductStore::open(&paths).unwrap();
        product.delete_views(&session).unwrap();
        drop(product);

        let (terminal, mut pilot) = launch(&fixture, "pilot-rebuilt", Some(&session));
        let rebuilt_tail = tail(&pilot.observe());
        quit(terminal, &mut pilot);
        let rebuilt = store.load_views(&session).unwrap().expect("rebuilt views");

        assert_eq!(rebuilt.rows, live.rows, "{shape}: rebuilt view rows");
        assert_eq!(
            rebuilt_tail, live_tail,
            "{shape}: the resumed TUI shows the rows the live TUI showed"
        );
    }
}
