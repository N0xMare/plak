use super::*;

/// Both faces announce on the descriptor their records belong on.
#[test]
fn each_face_announces_where_its_records_go() {
    assert_eq!(cli_announcement().fd, 2);
    assert_eq!(jsonl_announcement().fd, 1);
}

/// The JSONL announcement is exactly one error record and one `bye`, each a
/// complete line, so a reader that stops at `bye` sees a closed stream.
#[test]
fn the_jsonl_announcement_is_one_error_then_one_bye() {
    for bytes in [jsonl_announcement().terminated, jsonl_announcement().hangup] {
        let text = std::str::from_utf8(bytes).expect("utf-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        let error: serde_json::Value =
            serde_json::from_str(lines[0]).expect("error record is JSON");
        assert_eq!(error["kind"], "error");
        assert_eq!(error["v"], 1);
        assert!(
            error["message"].as_str().is_some_and(|m| !m.is_empty()),
            "the record says what happened"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[1]).expect("bye is JSON"),
            serde_json::json!({"kind": "bye", "v": 1})
        );
        assert!(text.ends_with('\n'), "every record is a complete line");
    }
}

/// The CLI announcement is one complete diagnostic line per signal.
#[test]
fn the_cli_announcement_is_one_line_per_signal() {
    for bytes in [cli_announcement().terminated, cli_announcement().hangup] {
        let text = std::str::from_utf8(bytes).expect("utf-8");
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.starts_with("plak: "));
        assert!(text.ends_with('\n'));
    }
}

/// A signalled exit reports the same status as an interrupted one.
#[test]
fn a_signalled_exit_shares_the_interrupted_status() {
    assert_eq!(SIGNALLED_EXIT_CODE, 4);
}

/// Each face announces on the discipline its stream allows.
///
/// The CLI speaks at once, because a diagnostic line costs nothing and the
/// operator may never see anything else. JSONL waits, because its stream
/// has room for exactly one terminal record and the graceful stop usually
/// writes it first.
#[test]
fn the_cli_speaks_at_once_and_jsonl_waits_for_its_turn() {
    assert!(cli_announcement().announce_immediately);
    assert!(!jsonl_announcement().announce_immediately);
}

/// The deadline sits between the graceful stop and the supervisor's grace.
///
/// Too short and a resumable session is cut off; too long and the process
/// outlives the two seconds a supervisor allows before `SIGKILL`. The
/// graceful path measures around 0.3 seconds on a live turn.
#[test]
fn the_deadline_leaves_room_for_the_graceful_stop_and_for_the_supervisor() {
    assert!(GRACEFUL_STOP_DEADLINE >= std::time::Duration::from_millis(600));
    assert!(GRACEFUL_STOP_DEADLINE <= std::time::Duration::from_millis(1_500));
}

/// A blocked TUI cannot exceed the same hard bound as voluntary quit.
#[test]
fn the_interactive_backstop_enforces_the_one_second_bound() {
    assert_eq!(
        INTERACTIVE_FORCED_EXIT_DEADLINE,
        std::time::Duration::from_secs(1)
    );
}

/// A signal racing a natural exit still yields exactly one `bye`, last.
///
/// Both terminators are real: the drive's tail writes `bye` on stdout, and
/// the signal thread writes an error record plus its own `bye` straight to
/// the descriptor. Only the claim keeps them from both landing, in either
/// arrival order.
#[test]
fn a_signal_racing_a_natural_exit_yields_exactly_one_bye_last() {
    let natural = b"{\"kind\":\"bye\",\"v\":1}\n";
    for signal_first in [false, true] {
        let flag = AtomicBool::new(false);
        let mut stream: Vec<u8> = Vec::new();
        let write_signal = |stream: &mut Vec<u8>| {
            if claim_terminal_record_flag(&flag) {
                stream.extend_from_slice(jsonl_announcement().terminated);
            }
        };
        let write_natural = |stream: &mut Vec<u8>| {
            if claim_terminal_record_flag(&flag) {
                stream.extend_from_slice(natural);
            }
        };
        if signal_first {
            write_signal(&mut stream);
            write_natural(&mut stream);
        } else {
            write_natural(&mut stream);
            write_signal(&mut stream);
        }
        let text = String::from_utf8(stream).expect("utf-8");
        let records: Vec<serde_json::Value> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("each record is JSON"))
            .collect();
        let byes = records
            .iter()
            .filter(|record| record["kind"] == "bye")
            .count();
        assert_eq!(byes, 1, "signal_first={signal_first}: {records:?}");
        assert_eq!(
            records.last().expect("a terminator")["kind"],
            "bye",
            "signal_first={signal_first}: the terminator is last: {records:?}"
        );
    }
}
