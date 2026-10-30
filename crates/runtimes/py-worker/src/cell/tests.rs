use super::*;

#[test]
fn a_terminal_declines_results_the_interpreter_never_resumed_on() {
    for outcome in [
        CellOutcome::Completed {
            value: None,
            stored: HashMap::new(),
        },
        CellOutcome::Failed {
            message: "bootstrap failed".into(),
            stored: HashMap::new(),
            generation_lost: true,
        },
    ] {
        // The interpreter waits on call 9, not call 7. Its result can
        // arrive first, but no ResumeFutures hands it to the interpreter.
        let mut worker = Worker {
            child: Child::default(),
            configuration: session_config(),
            bindings: None,
            running: Some(Running {
                cell: 1,
                futures: BTreeSet::from([7, 9]),
                waiting: Some(vec![9]),
                ..Running::default()
            }),
        };
        worker
            .nested_result(1, 7, Ok(CellValue::Null))
            .map_err(|lost| lost.message)
            .expect("the result is held, not resumed");
        let mut frames = Vec::new();
        worker
            .finish(outcome.clone(), |frame| {
                frames.push(frame.clone());
                Ok(())
            })
            .map_err(|lost| lost.message)
            .expect("terminal");
        assert!(matches!(
            frames.as_slice(),
            [WorkerFrame::Delivery {
                cell: 1,
                call: 7,
                verdict: DeliveryVerdict::NotObserved(_),
            }, WorkerFrame::CellDone { cell: 1, outcome: actual }] if actual == &outcome
        ));
        assert!(worker.running.is_none());
    }
}

#[test]
fn undecodable_traceback_is_a_definite_failure_that_loses_the_generation() {
    let exception = pb::RaisedException {
        exc_type: "RuntimeError".into(),
        message: Some("nested rejection".into()),
        traceback: vec![pb::StackFrame {
            filename: SCRIPT_NAME.into(),
            start: Some(pb::CodeLoc {
                line: 1,
                column: 12,
            }),
            end: Some(pb::CodeLoc { line: 1, column: 5 }),
            preview_line: Some("await fetch({})".into()),
            ..pb::StackFrame::default()
        }]
        .into(),
        data: None,
    };
    assert!(
        matches!(exception_outcome(exception), CellOutcome::Failed { generation_lost: true, ref message, .. } if message.starts_with(PLAKOUS_PY_PTC_ERROR_UNDECODABLE_TRACEBACK_PREFIX))
    );
    let ordinary = MontyException::new(ExcType::MemoryError, None);
    assert!(
        matches!(exception_outcome((&ordinary).into()), CellOutcome::Failed { generation_lost: false, ref message, .. } if message.contains("MemoryError"))
    );
}

#[test]
fn cumulative_print_cap_reports_truncation_once_on_a_char_boundary() {
    let mut printed = Printed::default();
    let first = printed
        .admit(&"é".repeat(MAX_PRINT_BYTES))
        .expect("the first print is admitted");
    assert!(first.len() <= MAX_PRINT_BYTES);
    assert!(first.ends_with("[Python output truncated at 1 MiB]\n"));
    let header = "Python cell running with cell ID 18446744073709551615\nOutput:\n";
    assert!(
        header.len() + first.len() <= 1_048_576,
        "the truncation notice must survive status-inclusive projection"
    );
    assert_eq!(printed.admit("more dropped output"), None);
}

#[test]
fn the_session_states_its_limits_and_os_policy_explicitly() {
    let config = session_config();
    let limits = config.limits.expect("resource limits");
    assert_eq!(limits.max_memory_bytes, Some(256 * 1024 * 1024));
    assert_eq!(
        limits.max_suspensions,
        Some(u64::MAX),
        "a finite suspension cap would count across the interpreter's life"
    );
    assert!(!config.type_check);
    assert_eq!(config.script_name, SCRIPT_NAME);
    let policy = config.os_policy.expect("OS policy");
    let expected: pb::OsPolicy = (&OsPolicy {
        datetime: DateTimeSource::System,
        random_start: RandomStart::System,
        sleep: SleepMode::CallHost,
        ..OsPolicy::default()
    })
        .into();
    assert_eq!(policy, expected);
}

/// The type checker stays linked for the SDK; the vendored typeshed
/// must keep checking the built-in modules and refuse unknown imports.
#[test]
fn new_builtin_modules_type_check_and_run_against_vendored_stubs() {
    let mut child = Child::default();
    let mut sink = TurnSink::default();
    let configure = pb::Configure {
        type_check: true,
        ..session_config()
    };
    child
        .handle(
            request(pb::parent_request::Kind::Configure(configure)),
            &mut sink,
        )
        .expect("configure");
    assert!(matches!(
        sink.turn_end.take().and_then(|event| event.kind),
        Some(pb::child_event::Kind::Ok(_))
    ));
    let feed = |code: &str| {
        request(pb::parent_request::Kind::Feed(pb::Feed {
            code: code.to_owned(),
            inputs: Vec::new().into(),
            values: None,
            skip_type_check: false,
            cwd: "/".to_owned(),
        }))
    };
    let source = r#"import csv
import hashlib
import tomllib
import bisect
import os.path
assert list(csv.reader(["a,b", "1,2"])) == [["a", "b"], ["1", "2"]]
assert hashlib.sha256(b"x").hexdigest() == "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
assert tomllib.loads("x = 1")["x"] == 1
assert bisect.bisect([1, 3, 5], 4) == 2
assert os.path.join("/a", "b") == "/a/b"
True"#;
    let mut next = feed(source);
    loop {
        child.handle(next, &mut sink).expect("type checked feed");
        match sink.turn_end.take().and_then(|event| event.kind) {
            Some(pb::child_event::Kind::Complete(complete)) => {
                let value = MontyObject::try_from(complete).expect("value");
                assert_eq!(format_monty(&value, 1024), "True");
                break;
            }
            Some(pb::child_event::Kind::OsCall(call)) => {
                let (call_id, function) = os_call_from_proto(call).expect("os call");
                assert_eq!(function.name(), "import_stubs");
                next = os_call_refusal(call_id, function.name());
            }
            other => panic!("unexpected built-in host request: {other:?}"),
        }
    }
    child
        .handle(feed("import plak_host_module"), &mut sink)
        .expect("unknown import requests stubs");
    let Some(pb::child_event::Kind::OsCall(call)) =
        sink.turn_end.take().and_then(|event| event.kind)
    else {
        panic!("expected import_stubs");
    };
    let (call_id, function) = os_call_from_proto(call).expect("os call");
    assert_eq!(function.name(), "import_stubs");
    child
        .handle(os_call_refusal(call_id, function.name()), &mut sink)
        .expect("no host stubs");
    let Some(pb::child_event::Kind::TypingError(typing)) =
        sink.turn_end.take().and_then(|event| event.kind)
    else {
        panic!("an unknown import is a typing error");
    };
    assert!(
        typing.diagnostics.contains("plak_host_module"),
        "{}",
        typing.diagnostics
    );
}
