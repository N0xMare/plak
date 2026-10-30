use super::*;

#[test]
fn capture_requires_private_directory_and_marks_truncation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("provider-authority");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let run = root.join("run");
    fs::create_dir(&run).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
    let dir = run.join("segment");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Capture::open(&dir, &root).is_err());
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let other = run.join("other");
    std::os::unix::fs::symlink(&dir, &other).unwrap();
    assert!(Capture::open(&other, &root).is_err());
    let mut capture = Capture::open(&dir, &root).unwrap();
    capture.max_run_bytes = 0; // Explicit qualification ignores automatic budgets.
    assert!(capture.required());
    let mut exchange = capture.start(b"private prompt").unwrap();
    assert_eq!(
        fs::read(dir.join("exchange-0001.request")).unwrap(),
        b"private prompt"
    );
    let pending = fs::read_to_string(dir.join("exchange-0001.info")).unwrap();
    assert!(pending.contains("terminal=pending"));
    assert!(pending.contains("incomplete=true"));
    for extension in ["request", "body", "info"] {
        assert_eq!(
            fs::metadata(dir.join(format!("exchange-0001.{extension}")))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    exchange.response_head(200);
    exchange.response_chunk(&vec![b'a'; MAX_RESPONSE_BYTES + 1]);
    assert!(exchange.failed());
    exchange.terminal(&ExchangeTerminal::Complete);
    assert_eq!(
        fs::metadata(dir.join("exchange-0001.body")).unwrap().len(),
        MAX_RESPONSE_BYTES as u64
    );
    assert!(
        fs::read_to_string(dir.join("exchange-0001.info"))
            .unwrap()
            .contains("incomplete=true")
    );
    assert_eq!(
        fs::metadata(dir.join("exchange-0001.body"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let mut complete = capture.start(b"synthetic request").unwrap();
    complete.response_head(200);
    complete.response_chunk(b"synthetic response");
    complete.terminal(&ExchangeTerminal::Complete);
    assert!(!complete.failed());
    assert!(
        fs::read_to_string(dir.join("exchange-0002.info"))
            .unwrap()
            .contains("incomplete=false")
    );
    assert!(capture.start(&vec![0; MAX_REQUEST_BYTES + 1]).is_none());
    *capture.next.lock().unwrap() = 512;
    assert!(capture.start(b"beyond old lifetime limit").is_some());
    assert!(dir.join("exchange-0001.request").exists());
    assert!(!dir.join("retention.info").exists());
}

#[test]
fn automatic_capture_is_fresh_private_and_rejects_insecure_roots() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("captures");
    let first = Capture::create(&root).expect("first capture");
    let second = Capture::create(&root).expect("second capture");
    assert_ne!(first.directory(), second.directory());
    for path in [
        first.directory(),
        first.directory().parent().unwrap(),
        &root,
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Capture::create(&root).is_err());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&root, &link).unwrap();
    assert!(Capture::create(&link).is_err());
}

#[test]
fn automatic_capture_rotates_beyond_512_and_persists_budget_gaps() {
    let temp = tempfile::tempdir().unwrap();
    let capture = Capture::automatic(temp.path()).unwrap();
    assert!(!capture.required());
    for _ in 0..513 {
        let mut exchange = capture.start(b"synthetic").unwrap();
        exchange.response_chunk(b"response");
        exchange.terminal(&ExchangeTerminal::Complete);
    }
    let last = capture.dir.join("segment-0004/exchange-0513.info");
    let info = fs::read_to_string(last).unwrap();
    assert!(info.contains("exchange=513"));
    assert!(info.contains(&format!("process_id={}", std::process::id())));
    assert!(info.contains("incomplete=false"));
    assert!(
        capture
            .dir
            .join("segment-0000/exchange-0001.request")
            .exists()
    );
    assert!(capture.reserved.lock().unwrap().bytes < MAX_RESPONSE_BYTES);
    // Charge protected evidence, not eligible completed segments.
    capture.reserved.lock().unwrap().bytes = MAX_RUN_BYTES * 2;
    assert!(capture.start(b"unrecorded").is_none());
    let gap = fs::read(capture.dir.join("incomplete.log")).unwrap();
    assert!(capture.start(b"still unrecorded").is_none());
    assert_eq!(gap, fs::read(capture.dir.join("incomplete.log")).unwrap());
    assert!(!capture.required());
}

#[test]
fn retention_preserves_live_runs_inflight_exchanges_and_unmanaged_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("captures");
    let live = Capture::create(&root).unwrap();
    let inflight = live.start(b"synthetic").unwrap();
    let live_path = live.dir.parent().unwrap().to_owned();
    drop(live);
    let unmanaged = root.join("plak-existing-evidence");
    fs::create_dir(&unmanaged).unwrap();
    fs::write(unmanaged.join("evidence"), b"keep").unwrap();
    for _ in 0..RETAIN_COMPLETED_RUNS + 3 {
        drop(Capture::create(&root).unwrap());
    }
    Capture::retain(&root).unwrap();
    assert!(live_path.exists());
    assert_eq!(fs::read(unmanaged.join("evidence")).unwrap(), b"keep");
    assert_eq!(
        fs::read_dir(&root).unwrap().count(),
        RETAIN_COMPLETED_RUNS + 2
    );
    drop(inflight);
    Capture::retain(&root).unwrap();
    assert!(
        live_path.exists(),
        "abandoned pending evidence must survive unlock"
    );
    assert_eq!(
        fs::read_dir(&root).unwrap().count(),
        RETAIN_COMPLETED_RUNS + 2
    );
}

#[test]
fn retention_preserves_expired_failed_crashed_and_gap_runs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("captures");
    let mut preserved = Vec::new();
    for kind in 0..6 {
        let capture = Capture::create(&root).unwrap();
        preserved.push(capture.dir.parent().unwrap().to_owned());
        let mut exchange = capture.start(b"synthetic request").unwrap();
        exchange.response_head(if kind == 1 { 500 } else { 200 });
        match kind {
            0 => {} // Crash: persisted pending marker, no terminal.
            2 => exchange.terminal(&ExchangeTerminal::ClosedEarly),
            3 => exchange.terminal(&ExchangeTerminal::Abandoned),
            4 => {
                exchange.response_chunk(&vec![0; MAX_RESPONSE_BYTES + 1]);
                exchange.terminal(&ExchangeTerminal::Complete);
            }
            _ => exchange.terminal(&ExchangeTerminal::Complete),
        }
        if kind == 5 {
            capture.diagnose("synthetic recording gap");
        }
        drop(exchange);
        drop(capture);
    }
    // An old clean success must expire while all older failure evidence survives.
    let clean = Capture::create(&root).unwrap();
    let clean_path = clean.dir.parent().unwrap().to_owned();
    let mut exchange = clean.start(b"synthetic success").unwrap();
    exchange.response_chunk(&[0xff, 0xfe]); // Raw evidence is not parsed as metadata.
    exchange.terminal(&ExchangeTerminal::ProtocolComplete);
    drop(exchange);
    assert!(Capture::successful_run(&clean_path).unwrap());
    drop(clean);
    for _ in 0..RETAIN_COMPLETED_RUNS {
        let capture = Capture::create(&root).unwrap();
        let mut exchange = capture.start(b"synthetic success").unwrap();
        exchange.terminal(&ExchangeTerminal::Complete);
        drop(exchange);
    }
    Capture::retain(&root).unwrap();
    assert!(!clean_path.exists());
    assert!(preserved.iter().all(|path| path.exists()));
    assert_eq!(
        fs::read_dir(root).unwrap().count(),
        RETAIN_COMPLETED_RUNS + 6
    );
}

#[test]
fn retention_metadata_inspection_preserves_unsafe_and_missing_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let capture = Capture::automatic(temp.path()).unwrap();
    let run = capture.dir.parent().unwrap();
    let mut exchange = capture.start(b"synthetic").unwrap();
    exchange.terminal(&ExchangeTerminal::Complete);
    drop(exchange);
    let info = capture.dir.join("segment-0000/exchange-0001.info");
    assert!(Capture::successful_run(run).unwrap());
    fs::set_permissions(&info, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!Capture::successful_run(run).unwrap());
    fs::set_permissions(&info, fs::Permissions::from_mode(0o600)).unwrap();
    let body = info.with_extension("body");
    fs::remove_file(&body).unwrap();
    assert!(!Capture::successful_run(run).unwrap());
    std::os::unix::fs::symlink(&info, &body).unwrap();
    assert!(!Capture::successful_run(run).unwrap());
}

#[test]
fn large_requests_roll_current_segment_before_count_limit() {
    let temp = tempfile::tempdir().unwrap();
    let mut capture = Capture::automatic(temp.path()).unwrap();
    // Production count and reservation sizes; reduce only the total
    // budget so maximum-sized requests hit byte pressure cheaply.
    capture.max_run_bytes = 4 * MAX_REQUEST_BYTES;
    let request = vec![b'x'; MAX_REQUEST_BYTES];
    for terminal in [
        ExchangeTerminal::Complete,
        ExchangeTerminal::ProtocolComplete,
    ] {
        for _ in 0..3 {
            let mut exchange = capture.start(&request).expect("capture keeps advancing");
            exchange.response_head(200);
            exchange.terminal(&terminal);
        }
    }
    assert_eq!(*capture.next.lock().unwrap(), 6);
    assert!(!capture.dir.join("segment-0000").exists());
    assert!(!capture.dir.join("segment-0001").exists());
    assert!(
        capture
            .dir
            .join("segment-0002/exchange-0005.request")
            .exists()
    );
    assert!(capture.dir.join("segment-0002/exchange-0006.info").exists());
    let summary = fs::read_to_string(capture.dir.join("retention.info")).unwrap();
    assert!(summary.contains("selected_successful_exchange_ranges=[(1, 4)]"));
    assert!(!capture.dir.join("incomplete.log").exists());
    assert!(capture.reserved.lock().unwrap().bytes <= capture.max_run_bytes);
}

#[test]
fn small_budget_reclaims_successes_and_keeps_later_failure() {
    let temp = tempfile::tempdir().unwrap();
    let mut capture = Capture::automatic(temp.path()).unwrap();
    capture.segment_exchanges = 2;
    capture.max_run_bytes = MAX_RESPONSE_BYTES + 3 * 1024 * 1024;
    let response = vec![b'x'; 1024 * 1024];
    for _ in 0..40 {
        let mut exchange = capture.start(b"request").unwrap();
        exchange.response_head(200);
        exchange.response_chunk(&response);
        exchange.terminal(&ExchangeTerminal::Complete);
    }
    let mut failure = capture.start(b"later failure request").unwrap();
    failure.response_head(500);
    failure.response_chunk(b"later failure body");
    failure.terminal(&ExchangeTerminal::Complete);
    drop(failure);
    for _ in 0..20 {
        let mut exchange = capture.start(b"request").unwrap();
        exchange.response_chunk(&response);
        exchange.terminal(&ExchangeTerminal::Complete);
    }
    assert!(!capture.dir.join("segment-0000").exists());
    assert_eq!(
        fs::read(capture.dir.join("segment-0020/exchange-0041.request")).unwrap(),
        b"later failure request"
    );
    assert_eq!(
        fs::read(capture.dir.join("segment-0020/exchange-0041.body")).unwrap(),
        b"later failure body"
    );
    let summary = capture.dir.join("retention.info");
    assert!(
        fs::read_to_string(&summary)
            .unwrap()
            .contains("full_run=false")
    );
    assert_eq!(
        fs::metadata(summary).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(capture.reserved.lock().unwrap().bytes < capture.max_run_bytes);
}

#[test]
fn diagnostic_warnings_are_bounded_to_one_summary_per_minute() {
    let mut diagnostics = Diagnostics::default();
    let now = std::time::Instant::now();
    assert_eq!(diagnostics.report("capacity", now), Some(0));
    assert_eq!(
        diagnostics.report("capacity", now + std::time::Duration::from_secs(1)),
        None
    );
    assert_eq!(
        diagnostics.report("capacity", now + std::time::Duration::from_secs(59)),
        None
    );
    assert_eq!(
        diagnostics.report("capacity", now + std::time::Duration::from_secs(60)),
        Some(2)
    );
}

#[test]
fn diagnostic_reasons_are_independent_and_flush_pending_counts() {
    let mut diagnostics = Diagnostics::default();
    let now = std::time::Instant::now();
    assert_eq!(diagnostics.report("capacity", now), Some(0));
    assert_eq!(diagnostics.report("io", now), Some(0));
    assert_eq!(diagnostics.report("capacity", now), None);
    assert_eq!(diagnostics.report("io", now), None);
    assert_eq!(diagnostics.report("capacity", now), None);
    assert_eq!(diagnostics.pending(), vec![("capacity", 2), ("io", 1)]);
    assert_eq!(diagnostics.pending(), Vec::new());
}

#[test]
fn dropping_capture_flushes_suppressed_diagnostics() {
    #[derive(Clone)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = Buffer(Arc::clone(&bytes));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    let home = tempfile::tempdir().unwrap();
    tracing::subscriber::with_default(subscriber, || {
        let capture = Capture::automatic(home.path()).unwrap();
        capture.diagnose("capacity");
        capture.diagnose("capacity");
        capture.diagnose("io");
        drop(capture);
    });
    let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert_eq!(
        output.matches("private wire evidence incomplete").count(),
        3
    );
    assert!(output.contains("final suppressed summary"));
    assert!(output.contains("suppressed=1"));
}

#[test]
fn failed_exchange_does_not_pin_successful_neighbors() {
    let home = tempfile::tempdir().unwrap();
    let mut capture = Capture::automatic(home.path()).unwrap();
    capture.segment_exchanges = 2;
    for terminal in [ExchangeTerminal::Abandoned, ExchangeTerminal::Complete] {
        let mut exchange = capture.start(b"request").unwrap();
        exchange.terminal(&terminal);
    }
    let mut budget = capture.reserved.lock().unwrap();
    capture
        .reclaim(&mut budget, 1, capture.max_run_bytes)
        .unwrap();
    assert!(
        capture
            .dir
            .join("segment-0000/exchange-0001.request")
            .exists()
    );
    assert!(
        !capture
            .dir
            .join("segment-0000/exchange-0002.request")
            .exists()
    );
    assert_eq!(budget.removed, vec![(2, 2)]);
    assert_eq!(budget.bytes, budget.segments[&0].exchanges[&1]);
}

#[test]
fn small_budget_protects_active_abandoned_and_incomplete_segments() {
    let temp = tempfile::tempdir().unwrap();
    let mut capture = Capture::automatic(temp.path()).unwrap();
    capture.segment_exchanges = 1;
    capture.max_run_bytes = 3 * MAX_RESPONSE_BYTES + 32 * 1024;
    let mut active = capture.start(b"active").unwrap();
    // A terminal does not release the active observation's protection.
    active.terminal(&ExchangeTerminal::Complete);
    let abandoned = capture.start(b"abandoned").unwrap();
    drop(abandoned);
    let mut incomplete = capture.start(b"incomplete").unwrap();
    incomplete.response_chunk(&vec![0; MAX_RESPONSE_BYTES + 1]);
    incomplete.terminal(&ExchangeTerminal::Complete);
    drop(incomplete);
    let mut closed = capture.start(b"failed physical exchange").unwrap();
    closed.terminal(&ExchangeTerminal::Abandoned);
    drop(closed);
    for _ in 0..6 {
        let mut exchange = capture.start(b"success").unwrap();
        exchange.terminal(&ExchangeTerminal::Complete);
    }
    for id in 0..4 {
        assert!(capture.dir.join(format!("segment-{id:04}")).exists());
    }
    assert!(capture.dir.join("retention.info").exists());
    capture.max_run_bytes = capture.reserved.lock().unwrap().bytes;
    assert!(capture.start(b"disk limit").is_none());
    assert!(capture.dir.join("incomplete.log").exists());
    assert!(!capture.required());
}

#[test]
fn missing_or_colliding_files_refuse_start() {
    let temp = tempfile::tempdir().unwrap();
    let capture = Capture {
        dir: temp.path().join("missing"),
        next: Mutex::new(0),
        required: true,
        run: None,
        reserved: Arc::new(Mutex::new(Budget::default())),
        max_run_bytes: MAX_RUN_BYTES,
        segment_exchanges: SEGMENT_EXCHANGES,
        diagnostics: Mutex::new(Diagnostics::default()),
    };
    assert!(capture.start(b"not dispatched").is_none());
    let capture = Capture {
        dir: temp.path().to_owned(),
        next: Mutex::new(0),
        required: true,
        run: None,
        reserved: Arc::new(Mutex::new(Budget::default())),
        max_run_bytes: MAX_RUN_BYTES,
        segment_exchanges: SEGMENT_EXCHANGES,
        diagnostics: Mutex::new(Diagnostics::default()),
    };
    let request = temp.path().join("exchange-0001.request");
    fs::write(&request, b"do not overwrite").unwrap();
    assert!(capture.start(b"not dispatched").is_none());
    assert_eq!(fs::read(request).unwrap(), b"do not overwrite");
    assert!(
        fs::read_to_string(temp.path().join("exchange-0001.info"))
            .unwrap()
            .contains("incomplete=true")
    );
}

#[test]
fn response_and_terminal_io_failures_are_required_failures() {
    let temp = tempfile::tempdir().unwrap();
    let body = temp.path().join("body");
    let info = temp.path().join("info");
    fs::write(&body, b"").unwrap();
    fs::write(&info, b"incomplete=true\n").unwrap();
    let mut exchange = ExchangeCapture {
        file: File::open(&body).unwrap(), // Read-only: deterministic write failure.
        info: OpenOptions::new().write(true).open(&info).unwrap(),
        bytes: 0,
        status: None,
        connection: None,
        failed: false,
        _run: None,
        budget: None,
        exchange: 1,
        segment: 0,
        complete: false,
    };
    exchange.response_chunk(b"response");
    assert!(exchange.failed());
    exchange.terminal(&ExchangeTerminal::Complete);
    assert!(
        fs::read_to_string(&info)
            .unwrap()
            .contains("incomplete=true")
    );
    let mut exchange = ExchangeCapture {
        file: OpenOptions::new().write(true).open(&body).unwrap(),
        info: File::open(&info).unwrap(), // Read-only: deterministic terminal failure.
        bytes: 0,
        status: None,
        connection: None,
        failed: false,
        _run: None,
        budget: None,
        exchange: 2,
        segment: 0,
        complete: false,
    };
    exchange.terminal(&ExchangeTerminal::Complete);
    assert!(exchange.failed());
    assert!(
        fs::read_to_string(info)
            .unwrap()
            .contains("incomplete=true")
    );
}
