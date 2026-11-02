//! Measurement facts of the real canonical tools, end to end into the store.

use super::*;

fn measured_tools_script() -> Value {
    let call = |name: &str, arguments: Value| json!({"output": {"tool": {"name": name, "arguments": arguments}}, "finish": "tool_calls"});
    json!({
        "version": 1,
        "process_tools": true,
        "steps": [
            call("read_file", json!({"path": "secret-dir/notes.txt", "offset": 2, "limit": 2})),
            call("read_file", json!({"path": "./secret-dir//notes.txt"})),
            call("search_repository", json!({"query": "needle-secret", "path": "."})),
            call(
                "search_repository",
                json!({"query": "", "path": "secret-dir", "list_directory": true})
            ),
            call("process_session_start", json!({
                "command": "PGPASSWORD=hunter2 sleep 3",
                "workdir": ".",
                "login": false,
                "tty": false,
                "keep_stdin_open": false,
                "yield_time_ms": 250,
                "max_output_bytes": 4096
            })),
            call("process_session_poll", json!({
                "session_id": 1,
                "yield_time_ms": 0,
                "max_output_bytes": 4096,
                "max_output_records": 16
            })),
            {"output": {"text": "measured"}}
        ]
    })
}

type ToolRow = (
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
);

fn tool_rows(connection: &rusqlite::Connection) -> Vec<ToolRow> {
    let mut statement = connection
        .prepare(
            "SELECT tool_name, access_digest, range_first_line, range_lines, page_partial,
                    command_class, wait_count, wait_kind, wait_outcome
             FROM tool_calls ORDER BY started_at",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Reads, searches, a listing, a process start and its poll each leave their
/// typed facts on their own `tool_calls` row. No path, query, command or
/// secret reaches the store, and the first attempt alone carries the request
/// footprint.
#[test]
fn real_tools_record_typed_measurements_and_no_content() {
    let fixture = Fixture::new(&measured_tools_script());
    std::fs::create_dir_all(fixture.workspace().join("secret-dir")).expect("dir");
    std::fs::write(
        fixture.workspace().join("secret-dir/notes.txt"),
        "one\ntwo\nthree needle-secret\nfour\nfive\n",
    )
    .expect("file");
    let run = run_cli(
        &fixture,
        &[
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--",
            "measure the tools",
        ],
    );
    assert!(run.status.success(), "{}\n{}", run.stdout, run.stderr);
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let connection = rusqlite::Connection::open(paths.telemetry_database()).unwrap();
    let rows = tool_rows(&connection);
    assert_eq!(rows.len(), 6, "{rows:?}\n{}\n{}", run.stdout, run.stderr);
    let (partial, whole, search, listing, start, poll) =
        (&rows[0], &rows[1], &rows[2], &rows[3], &rows[4], &rows[5]);
    let digest = partial.1.as_deref().expect("read digest");
    assert_eq!(digest.len(), 64);
    assert_eq!(whole.1.as_deref(), Some(digest), "./a//b reads a/b");
    assert_eq!(
        (partial.2, partial.3, partial.4),
        (Some(2), Some(2), Some(1))
    );
    assert_eq!((whole.2, whole.4), (Some(1), Some(0)));
    assert!(search.1.is_some() && search.1 != partial.1);
    assert_eq!(search.4, Some(0));
    assert!(listing.1.is_some() && listing.1 != search.1);
    assert_eq!(start.5.as_deref(), Some("sleep"));
    assert_eq!(start.1, None);
    assert_eq!(
        (poll.6, poll.7.as_deref(), poll.8.as_deref()),
        (Some(1), Some("process"), Some("timeout"))
    );
    // The finished job continues the session in a second turn; each turn's
    // first attempt alone carries the request footprint.
    let (turns, footprints): (i64, i64) = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM turns),
                    (SELECT COUNT(*) FROM model_attempts
                     WHERE instruction_bytes IS NOT NULL AND tool_schema_bytes > 0)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(turns >= 1);
    assert_eq!(footprints, turns, "only each turn's first attempt");
    let (attempts, gaps): (i64, i64) = connection
        .query_row(
            "SELECT COUNT(*), COUNT(idle_gap_ns) FROM model_attempts",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(attempts >= 7);
    assert_eq!(gaps, attempts - 1, "all but the driver's first request");
    drop(connection);
    let mut stored = std::fs::read(paths.telemetry_database()).unwrap();
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = paths.telemetry_database().into_os_string();
        sidecar.push(suffix);
        if let Ok(bytes) = std::fs::read(&sidecar) {
            stored.extend(bytes);
        }
    }
    let stored = String::from_utf8_lossy(&stored);
    for secret in [
        "secret-dir",
        "notes.txt",
        "needle-secret",
        "hunter2",
        "PGPASSWORD",
    ] {
        assert!(
            !stored.contains(secret),
            "{secret} reached the telemetry store"
        );
    }
}
