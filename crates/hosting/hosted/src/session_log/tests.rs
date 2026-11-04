#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;
use tempfile::tempdir;

/// The log needs a directory, not a product layout: the fixture used to
/// build a whole `PlakPaths` for one `sessions_dir()`, which is the
/// dependency the 3.4 split took out.
fn paths() -> (tempfile::TempDir, SessionHome) {
    let dir = tempdir().unwrap();
    let home = SessionHome::new(dir.path().join("sessions"));
    home.ensure().unwrap();
    (dir, home)
}

#[test]
fn a_suspended_holder_of_the_log_lock_never_blocks_an_open_or_append() {
    let (_dir, home) = paths();
    let id = "44444444-5555-6666-7777-888888888888";
    let log = SessionLog::open(&home, id).unwrap();
    log.append(SessionEvent::new("before")).unwrap();
    // A separate open file description is a separate lock owner, like a
    // stopped process that held the lock when it was suspended.
    let holder = File::open(home.session_events_file(id)).unwrap();
    holder.lock().unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    let opener = home.clone();
    std::thread::spawn(move || {
        let _ = sent.send(SessionLog::open(&opener, id).err());
    });
    let refused = received
        .recv_timeout(LOG_LOCK_WAIT * 4)
        .expect("an open waited on the held log lock past its bound");
    assert_eq!(refused.as_deref(), Some("session event log is busy"));
    let started = Instant::now();
    assert!(log.append(SessionEvent::new("while held")).is_err());
    assert!(started.elapsed() < LOG_LOCK_WAIT * 4);
    holder.unlock().unwrap();
    log.append(SessionEvent::new("after")).unwrap();
    assert_eq!(count_events(&home, id).unwrap(), 2);
}

#[test]
fn unknown_abandonment_claim_survives_tail_kind_filter_and_count_index() {
    let (_dir, home) = paths();
    let id = "33333333-4444-5555-6666-777777777777";
    let log = SessionLog::open(&home, id).unwrap();
    for claim in ["tool_round_completed", "future_unknown_claim"] {
        log.append(SessionEvent::new("turn.abandoned").str("claim", claim))
            .unwrap();
    }
    log.flush().unwrap();
    let lines = read_tail(&home, id, 10).unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        read_tail_for_kind(&home, id, 10, Some("turn.abandoned")).unwrap(),
        lines
    );
    assert_eq!(filter_kind(&lines, "turn"), lines);
    assert_eq!(count_events(&home, id).unwrap(), 2);
    assert_eq!(count_events_read_only(&home, id).unwrap(), 2);
    std::fs::remove_file(event_count_index_path(&home, id)).unwrap();
    assert_eq!(count_events(&home, id).unwrap(), 2);
    assert_eq!(
        serde_json::from_str::<Value>(&lines[0]).unwrap()["claim"],
        "tool_round_completed"
    );
}

/// Pins the record a schema rejection writes.
///
/// A live run recorded
/// `{"kind":"tool.rejected","name":"exec_command","reason":"InvalidArguments",
/// "arguments":null,"subject":null}` — the Debug of the error kind and two
/// null identifying fields, so the cause had to be recovered from the
/// runtime checkpoint store instead of read from the log.
#[test]
fn a_schema_rejection_names_the_tool_the_arguments_and_why() {
    let (_dir, home) = paths();
    let id = "33333333-4444-5555-6666-777777777777";
    let log = SessionLog::open(&home, id).unwrap();
    let reason = tool_rejected_reason("InvalidArguments", Some("wait does not accept cell_id"));
    log.append(tool_rejected_record(
        "wait",
        &reason,
        "call-1",
        Some("cell 2"),
        Some(r#"{"cell_id": 2}"#),
    ))
    .unwrap();
    let lines = read_tail(&home, id, 10).unwrap();
    let record: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(record["kind"], "tool.rejected");
    assert_eq!(record["name"], "wait");
    assert_eq!(record["call_id"], "call-1");
    assert_eq!(record["subject"], "cell 2");
    assert_eq!(
        record["arguments"], r#"{"cell_id": 2}"#,
        "the refused arguments must be recoverable from the log alone"
    );
    // The category is kept, so a reader grepping for it still finds it,
    // and the descriptive message says which constraint was missed.
    let reason = record["reason"].as_str().unwrap();
    assert!(reason.starts_with("InvalidArguments"), "{reason}");
    assert!(reason.contains("wait does not accept cell_id"), "{reason}");
    // With no descriptive message the record is what it always was.
    assert_eq!(
        tool_rejected_reason("InvalidArguments", None),
        "InvalidArguments"
    );
}

/// Pins the shape of the record a committed tool-result placeholders writes.
///
/// A reader of the log alone has to be able to tell how much was replaced
/// and what the projection cost before and after, so these four field
/// names and the kind are part of the product surface.
#[test]
fn a_committed_placeholders_writes_its_counts_under_a_stable_kind() {
    let (_dir, home) = paths();
    let id = "22222222-3333-4444-5555-666666666666";
    let log = SessionLog::open(&home, id).unwrap();
    log.append(
        SessionEvent::new("context.placeholders")
            .u64("results", 3)
            .u64("bytes", 122_880)
            .u64("tokens_before", 36_617)
            .u64("tokens_after", 852),
    )
    .unwrap();
    let lines = read_tail(&home, id, 10).unwrap();
    assert_eq!(lines.len(), 1);
    let record: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(record["kind"], "context.placeholders");
    assert_eq!(record["results"], 3);
    assert_eq!(record["bytes"], 122_880);
    assert_eq!(record["tokens_before"], 36_617);
    assert_eq!(record["tokens_after"], 852);
}

#[test]
fn append_and_tail_round_trip() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    let log = SessionLog::open(&home, id).unwrap();
    log.append(SessionEvent::new("session.created").str("id", id))
        .unwrap();
    log.append(
        SessionEvent::new("host.attached")
            .str("account", "codex")
            .str("model", "m")
            .bool("ok", true),
    )
    .unwrap();
    let lines = read_tail(&home, id, 10).unwrap();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("session.created"));
    assert!(lines[1].contains("host.attached"));
    assert_eq!(event_sequence(lines[1].as_bytes()), Some(2));
    log.flush().unwrap();
    EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
    assert_eq!(count_events(&home, id).unwrap(), 2);
    assert_eq!(
        EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get),
        0,
        "event-count reads after a flush must use the maintained sidecar"
    );
}

#[test]
fn log_paths_require_the_exact_canonical_product_uuid() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    for invalid in [
        "../outside",
        "11111111222233334444555555555555",
        "11111111-2222-3333-4444-55555555555A",
        "11111111-2222-3333-4444-555555555555.events",
    ] {
        assert!(SessionLog::open(&home, invalid).is_err(), "{invalid}");
        assert!(read_tail(&home, invalid, 1).is_err(), "{invalid}");
        assert!(count_events(&home, invalid).is_err(), "{invalid}");
    }
    assert!(!paths.sessions_dir().join("outside.events.jsonl").exists());
}

#[test]
fn parsed_kind_namespace_filter_applies_before_tail_limit() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    let log = SessionLog::open(&home, id).unwrap();
    log.append(SessionEvent::new("turn.user")).unwrap();
    log.append(SessionEvent::new("turn.user.delta")).unwrap();
    log.append(SessionEvent::new("other").str("detail", "turn.user"))
        .unwrap();
    log.append(SessionEvent::new("turn.user")).unwrap();

    let lines = read_tail_for_kind(&home, id, 2, Some("turn.user")).unwrap();
    assert_eq!(lines.len(), 2);
    assert!(
        lines
            .iter()
            .all(|line| event_kind_is(line.as_bytes(), "turn.user"))
    );
    let filtered = filter_kind(&read_tail(&home, id, 4).unwrap(), "turn.user");
    assert_eq!(&filtered[filtered.len() - 2..], lines);
}

#[test]
fn reopen_skips_empty_or_truncated_final_line_and_keeps_unique_sequence() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    {
        let log = SessionLog::open(&home, id).unwrap();
        log.append(SessionEvent::new("one")).unwrap();
        log.append(SessionEvent::new("two")).unwrap();
    }
    let path = home.session_events_file(id);
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"\n{\"ts\":0,\"seq\":3,\"kind\":\"cut")
        .unwrap();
    drop(file);

    let log = SessionLog::open(&home, id).unwrap();
    log.append(SessionEvent::new("after-reopen")).unwrap();
    let lines = read_tail(&home, id, 10).unwrap();
    let seqs = lines
        .iter()
        .filter_map(|line| event_sequence(line.as_bytes()))
        .collect::<Vec<_>>();
    assert_eq!(seqs, vec![1, 2, 3]);
    assert_eq!(count_events(&home, id).unwrap(), 3);
}

#[test]
fn reopen_fails_closed_when_bounded_corrupt_tail_hides_prior_sequence() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    let path = home.session_events_file(id);
    let mut file = File::create(path).unwrap();
    writeln!(file, "{{\"ts\":0,\"seq\":41,\"kind\":\"valid\"}}").unwrap();
    file.write_all(&vec![b'x'; MAX_SEQUENCE_RECOVERY_BYTES + 1])
        .unwrap();
    drop(file);
    assert_eq!(
        SessionLog::open(&home, id).err().as_deref(),
        Some("could not recover bounded session event sequence")
    );
}

#[test]
fn steady_append_never_reads_prior_log_and_open_recovery_is_bounded() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    let path = home.session_events_file(id);
    for scale in [1, 2, 4] {
        let mut file = File::create(&path).unwrap();
        for seq in 1..=scale * 4_096 {
            writeln!(file, "{{\"ts\":0,\"seq\":{seq},\"kind\":\"seed\"}}").unwrap();
        }
        drop(file);
        let before = std::fs::metadata(&path).unwrap().len();
        let log = SessionLog::open(&home, id).unwrap();
        for _ in 0..128 {
            log.append(SessionEvent::new("steady")).unwrap();
        }
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after > before);
        let mut file = File::open(&path).unwrap();
        let (_, recovery_bytes) =
            read_reverse_records(&mut file, usize::MAX, Some(MAX_SEQUENCE_RECOVERY_BYTES)).unwrap();
        assert!(recovery_bytes <= MAX_SEQUENCE_RECOVERY_BYTES);
    }
}

#[test]
fn fixed_tail_work_is_independent_of_total_log_size() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    let path = home.session_events_file(id);
    let mut observed = Vec::new();
    for scale in [1, 2, 4] {
        let mut file = File::create(&path).unwrap();
        for seq in 1..=scale * 8_192 {
            writeln!(file, "{{\"ts\":0,\"seq\":{seq},\"kind\":\"seed\"}}").unwrap();
        }
        drop(file);
        let mut file = File::open(&path).unwrap();
        let (tail, bytes) = read_reverse_records(&mut file, 16, None).unwrap();
        assert_eq!(tail.len(), 16);
        observed.push(bytes);
    }
    assert_eq!(observed[0], observed[1]);
    assert_eq!(observed[1], observed[2]);
}

#[test]
fn exact_count_streams_past_the_old_ten_thousand_limit() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    let path = home.session_events_file(id);
    let mut file = File::create(path).unwrap();
    for seq in 1..=10_017 {
        writeln!(file, "{{\"ts\":0,\"seq\":{seq},\"kind\":\"seed\"}}").unwrap();
    }
    assert_eq!(count_events(&home, id).unwrap(), 10_017);
}

#[test]
fn event_count_sidecar_rebuilds_once_then_scales_independently_of_log_size() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    let path = home.session_events_file(id);
    let mut file = File::create(&path).unwrap();
    for seq in 1..=24_017 {
        writeln!(file, "{{\"ts\":0,\"seq\":{seq},\"kind\":\"seed\"}}").unwrap();
    }
    drop(file);

    EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
    assert_eq!(count_events(&home, id).unwrap(), 24_017);
    assert!(EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get) > 0);

    EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
    for _ in 0..128 {
        assert_eq!(count_events(&home, id).unwrap(), 24_017);
    }
    assert_eq!(
        EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get),
        0,
        "fresh sidecar reads must not scan the event log"
    );

    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{{\"ts\":0,\"seq\":24018,\"kind\":\"new\"}}").unwrap();
    drop(file);
    assert_eq!(count_events(&home, id).unwrap(), 24_018);
    assert!(EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get) > 0);
}

#[test]
fn burst_appends_stay_ordered_and_well_formed() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    {
        let log = SessionLog::open(&home, id).unwrap();
        for index in 0..10_000_u64 {
            log.append(SessionEvent::new("burst").u64("index", index))
                .unwrap();
        }
    }
    let encoded = std::fs::read_to_string(home.session_events_file(id)).unwrap();
    let mut records = 0_u64;
    for (offset, line) in encoded.lines().enumerate() {
        let decoded: Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("record {offset} is malformed: {error}"));
        let offset = u64::try_from(offset).unwrap();
        assert_eq!(decoded["seq"].as_u64(), Some(offset + 1));
        assert_eq!(decoded["index"].as_u64(), Some(offset));
        assert_eq!(decoded["kind"].as_str(), Some("burst"));
        records += 1;
    }
    assert_eq!(records, 10_000);
    assert_eq!(count_events(&home, id).unwrap(), 10_000);
}

#[test]
fn appends_never_fsync_per_event_and_close_flushes_the_index() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    let log = SessionLog::open(&home, id).unwrap();
    EVENT_COUNT_TEST_INDEX_WRITES.with(|writes| writes.set(0));
    let started = Instant::now();
    for _ in 0..10_000 {
        log.append(SessionEvent::new("burst")).unwrap();
    }
    let elapsed = started.elapsed();
    let interval_flushes = elapsed.as_nanos() / SESSION_LOG_INDEX_INTERVAL.as_nanos();
    let during_appends = EVENT_COUNT_TEST_INDEX_WRITES.with(std::cell::Cell::get);
    assert!(
        u128::try_from(during_appends).unwrap() <= interval_flushes,
        "10,000 appends over {elapsed:?} performed {during_appends} index writes; at most \
             {interval_flushes} interval flushes are permitted and no per-event fsync"
    );

    drop(log);
    assert_eq!(
        EVENT_COUNT_TEST_INDEX_WRITES.with(std::cell::Cell::get),
        during_appends + 1,
        "close must persist the index exactly once"
    );
    EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
    assert_eq!(count_events(&home, id).unwrap(), 10_000);
    assert_eq!(
        EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get),
        0,
        "the close flush must leave an exact sidecar behind"
    );
}

/// The sidecar's `sync_all` and rename run after the log lock is released,
/// on every path that writes it: interval and close flushes, and a rebuild
/// on open or count. A slow disk never holds up a peer appender.
#[test]
fn the_count_sidecar_is_written_outside_the_log_lock() {
    let (_dir, home) = paths();
    let id = "55555555-6666-7777-8888-999999999999";
    EVENT_COUNT_TEST_INDEX_WRITES.with(|writes| writes.set(0));
    EVENT_COUNT_TEST_LOCKED_WRITES.with(|writes| writes.set(0));
    let log = SessionLog::open(&home, id).unwrap();
    log.append(SessionEvent::new("one")).unwrap();
    std::thread::sleep(SESSION_LOG_INDEX_INTERVAL);
    log.append(SessionEvent::new("two")).unwrap();
    log.append(SessionEvent::new("three")).unwrap();
    log.flush().unwrap();
    drop(log);
    std::fs::remove_file(event_count_index_path(&home, id)).unwrap();
    assert_eq!(count_events(&home, id).unwrap(), 3);
    std::fs::remove_file(event_count_index_path(&home, id)).unwrap();
    drop(SessionLog::open(&home, id).unwrap());
    assert!(event_count_index_path(&home, id).is_file());
    assert!(EVENT_COUNT_TEST_INDEX_WRITES.with(std::cell::Cell::get) >= 4);
    assert_eq!(EVENT_COUNT_TEST_LOCKED_WRITES.with(std::cell::Cell::get), 0);
}

#[test]
fn a_clean_flush_writes_once_and_a_second_flush_is_a_no_op() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    let log = SessionLog::open(&home, id).unwrap();
    log.append(SessionEvent::new("one")).unwrap();
    EVENT_COUNT_TEST_INDEX_WRITES.with(|writes| writes.set(0));
    log.flush().unwrap();
    log.flush().unwrap();
    assert_eq!(EVENT_COUNT_TEST_INDEX_WRITES.with(std::cell::Cell::get), 1);
    drop(log);
    assert_eq!(
        EVENT_COUNT_TEST_INDEX_WRITES.with(std::cell::Cell::get),
        1,
        "close must not rewrite an already-clean index"
    );
}

#[test]
fn a_session_switch_flushes_the_index_of_the_session_being_left() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let first = "11111111-2222-3333-4444-555555555555";
    let second = "11111111-2222-3333-4444-555555555556";
    let log = SessionLog::open(&home, first).unwrap();
    log.append(SessionEvent::new("one")).unwrap();
    log.append(SessionEvent::new("two")).unwrap();
    log.append_for(second, SessionEvent::new("elsewhere"))
        .unwrap();

    EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
    assert_eq!(count_events(&home, first).unwrap(), 2);
    assert_eq!(
        EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get),
        0,
        "the switch must have persisted the departed session's count"
    );
    log.flush().unwrap();
    assert_eq!(count_events(&home, second).unwrap(), 1);
}

#[test]
fn an_index_stale_against_a_longer_log_rescans_to_the_right_count() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    let path = home.session_events_file(id);
    {
        let log = SessionLog::open(&home, id).unwrap();
        for _ in 0..8 {
            log.append(SessionEvent::new("before-loss")).unwrap();
        }
    }
    let index_path = event_count_index_path(&home, id);
    let persisted =
        serde_json::from_slice::<EventCountIndex>(&std::fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(persisted.event_count, 8);

    // Simulate an abrupt loss after an interval flush: records reached the
    // log, the sidecar did not advance.
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    for seq in 9..=13 {
        writeln!(file, "{{\"ts\":0,\"seq\":{seq},\"kind\":\"after-loss\"}}").unwrap();
    }
    drop(file);

    EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
    assert_eq!(count_events(&home, id).unwrap(), 13);
    assert!(
        EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get) > 0,
        "a byte-length mismatch must force the bounded rescan"
    );

    // The rescan repairs the sidecar, and a deleted sidecar rebuilds too.
    EVENT_COUNT_TEST_SCANNED_BYTES.with(|bytes| bytes.set(0));
    assert_eq!(count_events(&home, id).unwrap(), 13);
    assert_eq!(EVENT_COUNT_TEST_SCANNED_BYTES.with(std::cell::Cell::get), 0);
    std::fs::remove_file(&index_path).unwrap();
    assert_eq!(count_events(&home, id).unwrap(), 13);
}

#[test]
fn independent_handles_resume_the_shared_sequence_and_count() {
    let (_dir, home) = paths();
    let id = "11111111-2222-3333-4444-666666666666";
    let first = SessionLog::open(&home, id).unwrap();
    let second = SessionLog::open(&home, id).unwrap();
    first.append(SessionEvent::new("host")).unwrap();
    second.append(SessionEvent::new("catalog.refresh")).unwrap();
    first.append(SessionEvent::new("host.again")).unwrap();
    second.flush().unwrap();
    first.flush().unwrap();
    let sequences: Vec<_> = read_tail(&home, id, 10)
        .unwrap()
        .iter()
        .map(|line| event_sequence(line.as_bytes()).unwrap())
        .collect();
    assert_eq!(sequences, [1, 2, 3]);
    assert_eq!(count_events(&home, id).unwrap(), 3);
    drop((first, second));
    let reopened = SessionLog::open(&home, id).unwrap();
    reopened.append(SessionEvent::new("reopened")).unwrap();
    reopened.flush().unwrap();
    assert_eq!(
        event_sequence(read_tail(&home, id, 1).unwrap()[0].as_bytes()),
        Some(4)
    );
    assert_eq!(count_events(&home, id).unwrap(), 4);
}

#[test]
fn a_stale_writer_flush_reconciles_before_publishing_the_index() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-666666666666";
    let path = home.session_events_file(id);
    let index_path = event_count_index_path(&home, id);

    let log = SessionLog::open(&home, id).unwrap();
    log.append(SessionEvent::new("host")).unwrap();
    // A second writer appends and leaves the log at sequence 2.
    {
        let second = SessionLog::open(&home, id).unwrap();
        second.append(SessionEvent::new("second")).unwrap();
    }
    // The host must catch up before publishing, despite its dirty cache.
    log.flush().unwrap();
    let persisted =
        serde_json::from_slice::<EventCountIndex>(&std::fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(
        persisted.log_bytes,
        path.metadata().unwrap().len(),
        "the projection describes the locked current prefix"
    );
    assert_eq!(persisted.event_count, 2);
    assert_eq!(persisted.last_sequence, 2);

    drop(log);
    assert_eq!(count_events(&home, id).unwrap(), 2);
    let repaired =
        serde_json::from_slice::<EventCountIndex>(&std::fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(repaired.event_count, 2);
    assert_eq!(repaired.last_sequence, 2);
}

#[test]
fn pre_coordination_index_is_rebuilt_even_when_duplicate_sequence_stamp_matches() {
    let (_dir, home) = paths();
    let id = "11111111-2222-3333-4444-666666666666";
    let path = home.session_events_file(id);
    let mut file = File::create(&path).unwrap();
    for seq in [1, 2, 2] {
        writeln!(file, "{{\"seq\":{seq},\"kind\":\"historical\"}}").unwrap();
    }
    write_event_count_index(
        &home,
        id,
        &EventCountIndex {
            format_revision: 2,
            log_bytes: file.metadata().unwrap().len(),
            event_count: 2,
            last_sequence: 2,
        },
    )
    .unwrap();
    assert_eq!(count_events(&home, id).unwrap(), 3);
    let repaired: EventCountIndex =
        serde_json::from_slice(&std::fs::read(event_count_index_path(&home, id)).unwrap()).unwrap();
    assert_eq!(repaired.format_revision, EVENT_COUNT_FORMAT_REVISION);
    assert_eq!(repaired.event_count, 3);
}

#[test]
fn concurrent_independent_handles_do_not_duplicate_or_interleave_records() {
    let (_dir, home) = paths();
    let id = "11111111-2222-3333-4444-666666666666";
    let ready = std::sync::Arc::new(std::sync::Barrier::new(4));
    std::thread::scope(|scope| {
        for writer in 0..4 {
            let ready = &ready;
            let home = &home;
            scope.spawn(move || {
                let log = SessionLog::open(home, id).unwrap();
                ready.wait();
                for event in 0..32 {
                    log.append(
                        SessionEvent::new("concurrent")
                            .u64("writer", writer)
                            .u64("event", event),
                    )
                    .unwrap();
                }
            });
        }
    });
    let lines = read_tail(&home, id, 128).unwrap();
    assert_eq!(lines.len(), 128);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(
            event_sequence(line.as_bytes()),
            Some(u64::try_from(index).unwrap() + 1)
        );
    }
    assert_eq!(count_events(&home, id).unwrap(), 128);
}

fn wait_for_test_file(path: &std::path::Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    path.is_file()
}

struct TestChild(std::process::Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn independent_processes_share_sequence() {
    const CHILD_HOME: &str = "PLAK_SESSION_LOG_TEST_CHILD_HOME";
    let id = "11111111-2222-3333-4444-666666666666";
    if let Some(path) = std::env::var_os(CHILD_HOME) {
        let home = SessionHome::new(path.into());
        let log = SessionLog::open(&home, id).unwrap();
        std::fs::write(home.sessions_dir().join("ready"), b"").unwrap();
        assert!(wait_for_test_file(&home.sessions_dir().join("start")));
        for _ in 0..32 {
            log.append(SessionEvent::new("child")).unwrap();
        }
        return;
    }
    let (_dir, home) = paths();
    let log = SessionLog::open(&home, id).unwrap();
    let mut child = TestChild(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session_log::tests::independent_processes_share_sequence",
                "--nocapture",
            ])
            .env(CHILD_HOME, home.sessions_dir())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(wait_for_test_file(&home.sessions_dir().join("ready")));
    // Both processes opened at zero; the child must see this later append.
    log.append(SessionEvent::new("before-child")).unwrap();
    std::fs::write(home.sessions_dir().join("start"), b"").unwrap();
    for _ in 0..32 {
        log.append(SessionEvent::new("parent")).unwrap();
    }
    // A liveness bound only: generous, so load never trips it.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "journal child did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    log.flush().unwrap();
    let lines = read_tail(&home, id, 65).unwrap();
    assert_eq!(lines.len(), 65);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(
            event_sequence(line.as_bytes()),
            Some(u64::try_from(index).unwrap() + 1)
        );
    }
    assert_eq!(count_events(&home, id).unwrap(), 65);
}

#[test]
fn truncates_long_unicode_strings_on_a_character_boundary() {
    let ev = SessionEvent::new("turn.user").str("text", "é".repeat(3_000));
    let line = String::from_utf8(ev.to_json_line(1, 0).unwrap()).unwrap();
    assert!(line.contains("chars]"));
    assert!(line.len() < 5_000);
}

#[test]
fn serde_encoding_keeps_every_event_on_one_valid_physical_line() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    let value = "line one\nline two\r\ttab\0control:\u{0001} unicode:界";
    let log = SessionLog::open(&home, id).unwrap();
    log.append(
        SessionEvent::new("tool.\nstarted")
            .str("odd\nkey", value)
            .str("text", value),
    )
    .unwrap();

    let encoded = std::fs::read_to_string(home.session_events_file(id)).unwrap();
    assert_eq!(encoded.lines().count(), 1);
    let decoded: Value = serde_json::from_str(encoded.trim_end()).unwrap();
    assert_eq!(decoded["odd\nkey"], value);
    assert_eq!(decoded["text"], value);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one table pins the small preview rule set"
)]
fn preview_redaction_rules_table() {
    // Token-shaped inputs are split with concat! so secret scanners do not
    // flag these synthetic values in the source.
    let cases = [
        ("API-key route", "API-key route"),
        ("tokenizer=cl100k", "tokenizer=cl100k"),
        ("input_tokens: 123", "input_tokens: 123"),
        ("max_tokens=4096", "max_tokens=4096"),
        (r#""input_tokens": 123"#, r#""input_tokens": 123"#),
        (r#""max_tokens": 4096"#, r#""max_tokens": 4096"#),
        (
            concat!(r#"{"api_key": ""#, "abcdefgh", "12345", r#""}"#),
            r#"{"api_key": "[redacted]"}"#,
        ),
        ("'token': 'abcdefgh'", "'token': '[redacted]'"),
        ("token_count", "token_count"),
        (
            "a password or token in prose",
            "a password or token in prose",
        ),
        ("git push -u origin main", "git push -u origin main"),
        ("xai-api", "xai-api"),
        ("[redacted]", "[redacted]"),
        (
            "[redacted credential-like content]",
            "[redacted credential-like content]",
        ),
        ("token=short", "token=short"),
        (concat!("sk-", "abcdefghijklmnopqrst"), "[redacted]"),
        (concat!("sk-ant-", "abcdefghijklmnopqrst"), "[redacted]"),
        (concat!("sk-proj-", "abcdefghijklmnopqrst"), "[redacted]"),
        (concat!("AKIA", "ABCDEFGHIJKLMNOP"), "[redacted]"),
        ("Bearer abcdefghijklmnop", "Bearer [redacted]"),
        ("bEaReR\tabcdefghijklmnop==", "bEaReR\t[redacted]"),
        ("Basic abcdefghijklmnop==", "Basic [redacted]"),
        ("token=abcdefgh", "token=[redacted]"),
        ("GITHUB_TOKEN=abcdefgh", "GITHUB_TOKEN=[redacted]"),
        ("OPENAI_API_KEY='abcdefgh'", "OPENAI_API_KEY='[redacted]'"),
        ("access_token: \"abcdefgh\"", "access_token: \"[redacted]\""),
        ("password=abcdefgh", "password=[redacted]"),
        (
            "API_TOKEN=\\\"hunter2hunter2\\\"",
            "API_TOKEN=\\\"[redacted]\\\"",
        ),
        ("passwd=abcdefgh", "passwd=[redacted]"),
        ("secret=abcdefgh", "secret=[redacted]"),
        (concat!("ghp_", "abcdefghijklmnopqrst"), "[redacted]"),
        (concat!("gho_", "abcdefghijklmnopqrst"), "[redacted]"),
        (concat!("ghs_", "abcdefghijklmnopqrst"), "[redacted]"),
        (concat!("ghu_", "abcdefghijklmnopqrst"), "[redacted]"),
        (concat!("ghr_", "abcdefghijklmnopqrst"), "[redacted]"),
        (concat!("github_pat_", "abcdefghijklmnopqrst"), "[redacted]"),
        (concat!("glpat-", "abcdefghijklmnopqrst"), "[redacted]"),
        (concat!("AIza", "abcdefghijklmnopqrst"), "[redacted]"),
        (concat!("xoxb-", "abcdefghijklmnopqrst"), "[redacted]"),
        (
            concat!(
                "xoxa-",
                "abcdefghijklmnopqrst ",
                "xoxp-",
                "abcdefghijklmnopqrst ",
                "xoxr-",
                "abcdefghijklmnopqrst ",
                "xoxs-",
                "abcdefghijklmnopqrst",
            ),
            "[redacted] [redacted] [redacted] [redacted]",
        ),
        (
            concat!(
                "eyJhbGciOiJIUzI1NiJ9",
                ".",
                "eyJzdWIiOiJ0ZXN0In0",
                ".signature"
            ),
            "[redacted]",
        ),
        (
            concat!(
                "-----BEGIN PRIVATE",
                " KEY-----\nsynthetic-test-material\n",
                "-----END PRIVATE",
                " KEY-----",
            ),
            "[redacted]",
        ),
        (
            "Authorization: Bearer short\nnext",
            "Authorization: [redacted]\nnext",
        ),
        (
            "Proxy-Authorization: Basic short",
            "Proxy-Authorization: [redacted]",
        ),
        (
            "curl -H 'X-Api-Key: short' https://safe.example",
            "curl -H 'X-Api-Key: [redacted]' https://safe.example",
        ),
        ("Cookie: name=short; other=value", "Cookie: [redacted]"),
        (
            "https://user:short@example.test/path",
            "https://user:[redacted]@example.test/path",
        ),
        (
            "https://example.test/?token=a&key=b&api_key=c&access_token=d&password=e&secret=f&sig=g&input_tokens=123",
            "https://example.test/?token=[redacted]&key=[redacted]&api_key=[redacted]&access_token=[redacted]&password=[redacted]&secret=[redacted]&sig=[redacted]&input_tokens=123",
        ),
    ];
    for (input, expected) in cases {
        let actual = redact_text_preview(input);
        assert_eq!(actual, expected, "{input}");
        assert_eq!(redact_text_preview(&actual), actual, "second pass: {input}");
        let json = redact_json_preview(&serde_json::json!({"value": input}));
        assert_eq!(
            serde_json::from_str::<Value>(&json).unwrap(),
            serde_json::json!({"value": expected}),
            "JSON leaf: {input}"
        );
    }
    let input = serde_json::json!({
        "nested": {"access_token": "short", "password": 123, "secret": false},
        "OPENAI_API_KEY": "short", "input_tokens": 123, "max_tokens": 4096,
        "token_count": 2, "tokenizer": "cl100k", "token": ["readable"],
        "password": ["hunter2hunter2"],
        "api_key": {"v": "hunter3hunter3", "n": 1, "ok": false}
    });
    let expected = serde_json::json!({
        "nested": {"access_token": "[redacted]", "password": "[redacted]", "secret": false},
        "OPENAI_API_KEY": "[redacted]", "input_tokens": 123, "max_tokens": 4096,
        "token_count": 2, "tokenizer": "cl100k", "token": ["[redacted]"],
        "password": ["[redacted]"],
        "api_key": {"v": "[redacted]", "n": "[redacted]", "ok": false}
    });
    let actual = redact_json_preview(&input);
    assert_eq!(serde_json::from_str::<Value>(&actual).unwrap(), expected);
    assert_eq!(redact_json_preview(&expected), actual);
    let input = format!("{}token=abcdefgh", "x ".repeat(995));
    let preview = redact_text_preview(&input);
    assert!(preview.contains("token=[red"), "{preview}");
    assert!(!preview.contains("abcdefgh"));
}

#[test]
fn persisted_previews_are_bounded_and_redact_nested_and_inline_secrets() {
    const SECRET: &str = "SECRET_SENTINEL_DO_NOT_RETAIN";
    let preview = redact_json_preview(&serde_json::json!({
        "path": "/tmp/useful",
        "nested": {"api_token": SECRET},
        "command": format!("curl -H Authorization: Bearer {SECRET}"),
        "large": "界".repeat(2_000),
    }));
    assert!(preview.contains("/tmp/useful"));
    assert!(!preview.contains(SECRET));
    assert!(preview.len() <= 2_050);

    for value in [
        format!("token={SECRET}"),
        format!("Bearer {SECRET}"),
        format!("sk-{SECRET}"),
        format!("curl -H 'X-API-Key: {SECRET}' https://safe.example/path"),
        format!("curl --header X_API_KEY={SECRET} https://safe.example/path"),
        format!("Authorization: Basic {SECRET}\n--safe flag"),
    ] {
        let redacted = redact_text_preview(&value);
        assert!(!redacted.contains(SECRET), "{value:?} => {redacted:?}");
        if value.contains("safe") {
            assert!(redacted.contains("safe"));
        }
        assert!(redacted.len() <= 2_050);
    }

    let headers = redact_json_preview(&serde_json::json!({
        "headers": {"X_API_KEY": SECRET, "authorization": format!("Bearer {SECRET}"), "Content-Type": "application/json"},
        "x-api-key": SECRET,
        "safe_path": "/tmp/retained"
    }));
    assert!(!headers.contains(SECRET));
    assert!(headers.contains("application/json"));
    assert!(headers.contains("/tmp/retained"));
}

#[test]
fn redaction_is_idempotent_and_never_accretes_punctuation() {
    // The argument dump reached the operator as
    // `"max_output_tokens": "[redacted]]]]"`. The marker itself ends in
    // `]`, and the punctuation-preserving rewrite treated that `]` as
    // structure to keep, so every extra pass appended one more.
    let arguments = serde_json::json!({
        "command": ["ls", "-la"],
        "max_output_tokens": 4096,
        "timeout_ms": 30_000,
        "api_key": "synthetic-secret",
    });
    let once = redact_json_preview(&arguments);
    assert!(once.contains("\"[redacted]\""), "{once}");

    let mut repeated = once.clone();
    for pass in 1..=4 {
        repeated = redact_text_preview(&repeated);
        assert_eq!(
            repeated, once,
            "pass {pass} changed an already-redacted preview"
        );
    }
    assert!(
        !repeated.contains("[redacted]]"),
        "no stray bracket may accrete:\n{repeated}"
    );
    // The structure the rewrite exists to preserve still survives.
    assert!(repeated.contains("\"-la\""), "{repeated}");
    assert!(
        serde_json::from_str::<Value>(&repeated).is_ok(),
        "the preview stays parseable JSON:\n{repeated}"
    );

    // A real secret is still redacted on the first pass and stable after.
    let secret = "s3cr3t-value-that-must-never-appear";
    let text = format!("curl -H 'X-API-Key: {secret}' https://safe.example/path");
    let first = redact_text_preview(&text);
    assert!(!first.contains(secret));
    assert_eq!(redact_text_preview(&first), first);
}

#[test]
fn malformed_physical_records_have_bounded_forward_and_reverse_working_sets() {
    let (_dir, paths) = paths();
    let home = paths.clone();
    let id = "11111111-2222-3333-4444-555555555555";
    let path = home.session_events_file(id);
    let mut file = File::create(&path).unwrap();
    writeln!(file, "{{\"ts\":0,\"seq\":1,\"kind\":\"valid\"}}").unwrap();
    file.write_all(&vec![
        b'x';
        MAX_REVERSE_SCAN_BYTES + MAX_PHYSICAL_RECORD_BYTES
    ])
    .unwrap();
    writeln!(file).unwrap();
    writeln!(file, "{{\"ts\":0,\"seq\":2,\"kind\":\"tail\"}}").unwrap();
    drop(file);

    assert_eq!(
        count_events(&home, id).unwrap_err(),
        "session event count encountered an oversized record"
    );
    assert_eq!(
        read_tail(&home, id, 2).unwrap_err(),
        "session event tail exceeds the bounded reverse scan"
    );
    let mut file = File::open(&path).unwrap();
    let (tail, scanned) = read_reverse_records(&mut file, 2, None).unwrap();
    assert_eq!(tail.len(), 1);
    assert!(tail[0].contains("\"seq\":2"));
    assert!(scanned <= MAX_REVERSE_SCAN_BYTES);

    let mut reader = BufReader::new(File::open(path).unwrap());
    let mut record = Vec::new();
    assert_eq!(
        read_bounded_record(&mut reader, &mut record).unwrap(),
        Some(true)
    );
    assert_eq!(
        read_bounded_record(&mut reader, &mut record).unwrap(),
        Some(false)
    );
    assert!(record.len() <= MAX_PHYSICAL_RECORD_BYTES);
}
