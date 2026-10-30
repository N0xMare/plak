use super::*;

#[test]
fn notice_classification_table_is_pinned() {
    use NoticePlacement::{GroupItem, NoticeRow, SessionLog};
    // (scope, placement, persisted with the session)
    let table = [
        (NoticeScope::Retry, GroupItem, true),
        (NoticeScope::Clipped, GroupItem, false),
        (NoticeScope::Turn, NoticeRow, true),
        (NoticeScope::Queue, NoticeRow, true),
        (NoticeScope::Recovery, NoticeRow, true),
        (NoticeScope::Tree, NoticeRow, true),
        (NoticeScope::Compaction, NoticeRow, true),
        (NoticeScope::Session, NoticeRow, true),
        (NoticeScope::Settings, NoticeRow, false),
        (NoticeScope::Approval, NoticeRow, false),
        (NoticeScope::Host, NoticeRow, true),
        (NoticeScope::Diagnostic, SessionLog, false),
    ];
    for (scope, placement, persisted) in table {
        assert_eq!(scope.placement(), placement, "{scope:?}");
        assert_eq!(scope.persisted(), persisted, "{scope:?}");
    }
}

/// `exposed_name` is present when the model called the tool by another
/// name and omitted when it equals `name`; the row is labelled by it.
#[test]
fn tool_item_exposed_name_round_trips_and_is_omitted_when_absent() {
    let item = |exposed_name: Option<&str>| ToolItem {
        call_id: "c1".into(),
        name: "apply_patch".into(),
        exposed_name: exposed_name.map(str::to_owned),
        kind: super::super::presentation::ActivityKind::Tool,
        state: "running".into(),
        subject: None,
        detail: None,
        started_at_ms: None,
        finished_at_ms: None,
        parent_call_id: None,
        cell_id: None,
        wait: None,
    };
    let aliased = TranscriptRow::Tool(item(Some("edit")));
    let value = serde_json::to_value(&aliased).unwrap();
    assert_eq!(value["exposed_name"], "edit");
    assert_eq!(
        serde_json::from_value::<TranscriptRow>(value).unwrap(),
        aliased
    );
    let plain = TranscriptRow::Tool(item(None));
    let value = serde_json::to_value(&plain).unwrap();
    assert!(value.get("exposed_name").is_none(), "{value}");
    assert_eq!(
        serde_json::from_value::<TranscriptRow>(value).unwrap(),
        plain
    );
    assert_eq!(item(Some("edit")).label(), "edit");
    assert_eq!(item(None).label(), "apply_patch");
}

#[test]
fn rows_round_trip_as_tagged_json() {
    let rows = [
        TranscriptRow::SessionHeader {
            session_id: "0a1b2c3d".into(),
            carried_from: Some(CarriedFrom {
                session_id: "4e5f6a7b".into(),
                state_key: "r21.e19.p27.t2".into(),
            }),
        },
        TranscriptRow::user("go"),
        TranscriptRow::User {
            text: "go".into(),
            sent_at_ms: Some(1_790_000_000_000),
        },
        TranscriptRow::steer("s"),
        TranscriptRow::Steer {
            text: "s".into(),
            sent_at_ms: Some(1_790_000_060_000),
        },
        TranscriptRow::assistant("ok"),
        TranscriptRow::Retry(RetryItem {
            attempt: 2,
            max_attempts: 4,
            cause: "429 rate limited".into(),
        }),
        TranscriptRow::AutoContinue(AutoContinue {
            agents_finished: 2,
            processes_exited: 1,
            agents: vec!["c1".into(), "c2".into()],
        }),
        TranscriptRow::Process(ProcessNotice {
            job_id: "12".into(),
            outcome: ProcessOutcome::Exited { code: Some(0) },
            duration_ms: Some(62_000),
            subject: None,
        }),
        TranscriptRow::Process(ProcessNotice {
            job_id: "p261".into(),
            outcome: ProcessOutcome::Stopped,
            duration_ms: None,
            subject: Some("just check".into()),
        }),
        TranscriptRow::Milestone(AgentMilestone {
            agent_id: "c23".into(),
            event: MilestoneEvent::AdoptConflict,
            template: None,
            text: None,
            duration_ms: None,
            candidate: Some("cand-c23-a1".into()),
            files: None,
            paths: vec!["src/lib.rs".into()],
        }),
        TranscriptRow::error(NoticeScope::Turn, "boom"),
        TranscriptRow::Interrupted,
        TranscriptRow::Truncated,
    ];
    for row in rows {
        let json = serde_json::to_string(&row).unwrap();
        assert_eq!(serde_json::from_str::<TranscriptRow>(&json).unwrap(), row);
    }
    assert_eq!(
        serde_json::to_string(&TranscriptRow::user("go")).unwrap(),
        r#"{"kind":"user","text":"go"}"#
    );
}

#[test]
fn send_time_and_process_subject_are_optional_fields() {
    assert_eq!(
        serde_json::to_string(&TranscriptRow::User {
            text: "go".into(),
            sent_at_ms: Some(7),
        })
        .unwrap(),
        r#"{"kind":"user","text":"go","sent_at_ms":7}"#
    );
    assert_eq!(
        serde_json::to_string(&TranscriptRow::Steer {
            text: "s".into(),
            sent_at_ms: Some(8),
        })
        .unwrap(),
        r#"{"kind":"steer","text":"s","sent_at_ms":8}"#
    );
    let process = TranscriptRow::Process(ProcessNotice {
        job_id: "p2".into(),
        outcome: ProcessOutcome::Exited { code: Some(0) },
        duration_ms: Some(5),
        subject: Some("make".into()),
    });
    assert_eq!(
        serde_json::to_string(&process).unwrap(),
        r#"{"kind":"process","job_id":"p2","outcome":{"exited":{"code":0}},"duration_ms":5,"subject":"make"}"#
    );
    // Rows written before the fields existed still read, without them.
    assert_eq!(
        serde_json::from_str::<TranscriptRow>(r#"{"kind":"steer","text":"s"}"#).unwrap(),
        TranscriptRow::steer("s")
    );
    assert!(matches!(
        serde_json::from_str::<TranscriptRow>(
            r#"{"kind":"process","job_id":"p2","outcome":"stopped"}"#
        )
        .unwrap(),
        TranscriptRow::Process(ProcessNotice { subject: None, .. })
    ));
}

#[test]
fn group_enders_match_the_spec() {
    assert!(TranscriptRow::error(NoticeScope::Retry, "x").ends_group());
    assert!(!TranscriptRow::info(NoticeScope::Retry, "x").ends_group());
    assert!(!TranscriptRow::info(NoticeScope::Clipped, "x").ends_group());
    assert!(TranscriptRow::info(NoticeScope::Turn, "x").ends_group());
    assert!(TranscriptRow::Interrupted.ends_group());
    // A stored auto-continue row is not shown, so it ends nothing.
    assert!(!TranscriptRow::AutoContinue(AutoContinue::default()).ends_group());
    assert!(
        !TranscriptRow::Retry(RetryItem {
            attempt: 2,
            max_attempts: 4,
            cause: "c".into()
        })
        .ends_group()
    );
}
