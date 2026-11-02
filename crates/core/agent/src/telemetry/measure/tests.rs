use super::*;
use plakous_tools::{FileReadValue, ListingValue, ProcessValue, SearchHitsValue};

fn read(path: &str, eof: bool, lines_cut: u64) -> ExecutionValue {
    ExecutionValue::FileRead(FileReadValue {
        path: path.to_owned(),
        text: "secret body".to_owned(),
        first_line: 11,
        lines: 20,
        eof,
        lines_cut,
        clipped_lines: 0,
        lossy: false,
        revision: None,
    })
}

fn process(status: &str, program: Option<&str>) -> ExecutionValue {
    ExecutionValue::Process(ProcessValue {
        job_id: 1,
        omitted_bytes: 0,
        status: status.to_owned(),
        exit_code: None,
        session_id: Some(1),
        output: "secret output".to_owned(),
        truncated: false,
        full_output: None,
        program: program.map(str::to_owned),
    })
}

fn measure(canonical: &str, value: &ExecutionValue) -> ToolMeasurement {
    measure_tool_call(
        Some(&[7; 32]),
        canonical,
        TelemetryTerminalStatus::Completed,
        Some(value),
        None,
    )
}

#[test]
fn read_digest_is_lexical_salted_and_hides_the_path() {
    let first = measure(
        READ_FILE_CAPABILITY_NAME,
        &read("src\\secret.txt", false, 0),
    );
    for same in ["./src/secret.txt", "src//secret.txt/", " src/./secret.txt"] {
        assert_eq!(
            measure(READ_FILE_CAPABILITY_NAME, &read(same, true, 0)).access_digest,
            first.access_digest,
            "{same}"
        );
    }
    assert_ne!(
        measure(READ_FILE_CAPABILITY_NAME, &read("src/other.txt", true, 0)).access_digest,
        first.access_digest
    );
    let other_salt = measure_tool_call(
        Some(&[8; 32]),
        READ_FILE_CAPABILITY_NAME,
        TelemetryTerminalStatus::Completed,
        Some(&read("src/secret.txt", true, 0)),
        None,
    );
    assert_ne!(other_salt.access_digest, first.access_digest);
    assert_eq!(first.range_first_line, Some(11));
    assert_eq!(first.range_lines, Some(20));
    let rendered = format!("{first:?}");
    assert!(!rendered.contains("secret"));
}

#[test]
fn read_page_partial_follows_the_typed_page() {
    let partial = |eof, cut| measure(READ_FILE_CAPABILITY_NAME, &read("a", eof, cut)).page_partial;
    assert_eq!(partial(false, 0), Some(true));
    assert_eq!(partial(true, 3), Some(true));
    assert_eq!(partial(true, 0), Some(false));
}

#[test]
fn no_salt_stores_no_digest() {
    let measured = measure_tool_call(
        None,
        READ_FILE_CAPABILITY_NAME,
        TelemetryTerminalStatus::Completed,
        Some(&read("a", true, 0)),
        None,
    );
    assert_eq!(measured.access_digest, None);
    assert_eq!(measured.page_partial, Some(false));
}

#[test]
fn searches_and_listings_digest_their_own_query_and_path() {
    let search = |query: &str, cut, incomplete| {
        measure(
            SEARCH_REPOSITORY_CAPABILITY_NAME,
            &ExecutionValue::SearchHits(SearchHitsValue {
                query: query.to_owned(),
                hits: Vec::new(),
                hits_cut: cut,
                incomplete,
            }),
        )
    };
    let listing = |path: &str, cut, incomplete| {
        measure(
            LIST_ENTRIES_CAPABILITY_NAME,
            &ExecutionValue::Listing(ListingValue {
                path: path.to_owned(),
                entries: Vec::new(),
                entries_cut: cut,
                incomplete,
            }),
        )
    };
    assert_eq!(
        search("needle", 0, false).access_digest,
        search("needle", 4, false).access_digest
    );
    assert_eq!(search("needle", 0, false).page_partial, Some(false));
    assert_eq!(search("needle", 1, false).page_partial, Some(true));
    assert_eq!(search("needle", 0, true).page_partial, Some(true));
    assert_eq!(
        listing("./crates", 0, false).access_digest,
        listing("crates/", 0, false).access_digest
    );
    assert_eq!(listing("crates", 2, false).page_partial, Some(true));
    assert!(listing("crates", 0, false).range_first_line.is_none());
}

#[test]
fn process_start_keeps_only_a_closed_program_label() {
    let class = |program| {
        measure(
            PROCESS_SESSION_CAPABILITY_NAME,
            &process("running", program),
        )
        .command_class
        .map(|class| class.as_str().to_owned())
    };
    assert_eq!(class(Some("cargo")).as_deref(), Some("cargo"));
    for leak in [
        Some("PGPASSWORD=hunter2"),
        Some("/usr/bin/cargo"),
        Some("my prog"),
        Some("12345"),
        Some("--token"),
        None,
    ] {
        assert_eq!(class(leak).as_deref(), Some("other"), "{leak:?}");
    }
    assert!(ProgramBasename::from_basename("a".repeat(33).as_str()).is_none());
    assert!(ProgramBasename::from_basename(&"a".repeat(32)).is_some());
    for token in [
        "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        "ghp_abcdef",
        "sk-proj-x",
        "xoxb-1",
        "AKIAIOSFODNN7EXAMPLE",
        "a1b2c3d4e5f6a7b8c9d0",
    ] {
        assert!(ProgramBasename::from_basename(token).is_none(), "{token}");
        assert_eq!(class(Some(token)).as_deref(), Some("other"), "{token}");
    }
}

#[test]
fn process_waits_end_on_their_deadline_only_while_running() {
    let outcome = |status| measure(PROCESS_SESSION_POLL_NAME, &process(status, None)).wait_outcome;
    assert_eq!(outcome("running"), Some(WaitOutcome::Timeout));
    assert_eq!(outcome("leftover"), Some(WaitOutcome::Timeout));
    assert_eq!(outcome("exited"), Some(WaitOutcome::Completed));
    assert_eq!(outcome("timed_out"), Some(WaitOutcome::Completed));
    let write = measure(PROCESS_SESSION_WRITE_NAME, &process("exited", None));
    assert_eq!(write.wait_count, Some(1));
    assert_eq!(write.wait_kind, Some(WaitTargetKind::Process));
    let cancelled = measure_tool_call(
        Some(&[1; 32]),
        PROCESS_SESSION_POLL_NAME,
        TelemetryTerminalStatus::Cancelled,
        None,
        None,
    );
    assert_eq!(cancelled.wait_outcome, Some(WaitOutcome::Cancelled));
    assert_eq!(
        measure(PROCESS_SESSION_CAPABILITY_NAME, &process("running", None)).wait_kind,
        None
    );
}

#[test]
fn wait_facts_map_every_wake_and_count_every_target() -> Result<(), plakous_tools::ToolRuntimeError>
{
    let wait = |fact, status| {
        let outcome = ToolOutcome::success_text("{\"mail\":[{\"body\":\"hidden\"}]}")
            .map(|outcome| outcome.with_wait_fact(fact));
        outcome.map(|outcome| measure_tool_call(None, "wait_agent", status, None, Some(&outcome)))
    };
    for (wake, expected) in [
        (WaitWake::Settled, WaitOutcome::Completed),
        (WaitWake::Mail, WaitOutcome::Mail),
        (WaitWake::Timeout, WaitOutcome::Timeout),
        (WaitWake::Interrupted, WaitOutcome::Interrupted),
    ] {
        let measured = wait(
            WaitFact::new(2, 1, wake),
            TelemetryTerminalStatus::Completed,
        )?;
        assert_eq!(measured.wait_outcome, Some(expected));
        assert_eq!(measured.wait_count, Some(3));
        assert_eq!(measured.wait_kind, Some(WaitTargetKind::Agent));
        assert!(!format!("{measured:?}").contains("hidden"));
    }
    let processes = wait(
        WaitFact::new(0, 4, WaitWake::Timeout),
        TelemetryTerminalStatus::Completed,
    )?;
    assert_eq!(processes.wait_kind, Some(WaitTargetKind::Process));
    assert_eq!(processes.wait_count, Some(4));
    let cancelled = wait(
        WaitFact::new(1, 0, WaitWake::Settled),
        TelemetryTerminalStatus::Cancelled,
    )?;
    assert_eq!(cancelled.wait_outcome, Some(WaitOutcome::Cancelled));
    Ok(())
}

#[test]
fn tools_outside_the_measured_classes_measure_nothing() {
    assert_eq!(
        measure_tool_call(
            Some(&[1; 32]),
            "apply_patch",
            TelemetryTerminalStatus::Completed,
            None,
            None
        ),
        ToolMeasurement::absent()
    );
    assert!(!measures_capability("apply_patch"));
    assert!(measures_capability(READ_FILE_CAPABILITY_NAME));
}
