//! The session's process rows reach its faces as one observation,
//! published only when they change.

use plakous_host_protocol::views::{ProcessStatus, ProcessView};

use super::Sender;
use crate::runtime::test_kit::TestExtension;
use crate::surface_protocol::{ContentEvent, HostEvent};

fn row(status: ProcessStatus, elapsed_ms: u64) -> ProcessView {
    ProcessView {
        id: "p1".into(),
        command: "make".into(),
        owner: "root".into(),
        started_at: 1,
        elapsed_ms,
        status,
        output: String::new(),
    }
}

/// A change publishes the rows and how many are live; no rows before the
/// first, the same rows, or a running row's elapsed time within its
/// second publish nothing; a session opening clears what was published,
/// so the next rows go out even when they are the same. A clear opens the
/// next session with no opening: held rows are replaced by an empty list
/// right after it, and the next session's own rows go out as a change.
#[test]
fn process_rows_are_published_when_they_change() {
    let (wire, received) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(wire);
    let published = || {
        received
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::Processes { views, running } => Some((views.len(), running)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    events.publish_processes(Vec::new());
    assert_eq!(published(), []);
    events.publish_processes(vec![row(ProcessStatus::Running, 100)]);
    assert_eq!(published(), [(1, 1)]);
    events.publish_processes(vec![row(ProcessStatus::Running, 900)]);
    assert_eq!(published(), []);
    events.publish_processes(vec![row(ProcessStatus::Running, 1_100)]);
    assert_eq!(published(), [(1, 1)]);
    events.publish_processes(vec![row(ProcessStatus::Exited(Some(0)), 1_100)]);
    assert_eq!(published(), [(1, 0)]);
    let _ = events.send(ContentEvent::SessionOpening);
    events.publish_processes(vec![row(ProcessStatus::Exited(Some(0)), 1_100)]);
    assert_eq!(published(), [(1, 0)]);
    events.publish_processes(Vec::new());
    assert_eq!(published(), [(0, 0)], "an emptied list is published");
    let _ = events.send(ContentEvent::ClearView);
    assert_eq!(published(), [], "a clear over no rows publishes none");
    events.publish_processes(vec![row(ProcessStatus::Running, 100)]);
    assert_eq!(published(), [(1, 1)]);
    let _ = events.send(ContentEvent::ClearView);
    assert_eq!(published(), [(0, 0)], "a clear empties the held rows");
    events.publish_processes(Vec::new());
    assert_eq!(published(), []);
    events.publish_processes(vec![row(ProcessStatus::Running, 100)]);
    assert_eq!(
        published(),
        [(1, 1)],
        "the next session's rows are a change"
    );
}

/// A published row is redacted as a tool row is: a secret the model typed
/// into a command, or one a process printed, never reaches a face.
#[test]
fn published_process_rows_are_redacted() {
    let (wire, received) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(wire);
    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
    events.publish_processes(vec![ProcessView {
        command: format!("curl -H 'Authorization: Bearer {secret}' https://example.test"),
        output: format!("token={secret}\n"),
        ..row(ProcessStatus::Running, 100)
    }]);
    let published = received
        .try_iter()
        .find_map(|event| match event {
            HostEvent::Processes { views, .. } => Some(views),
            _ => None,
        })
        .expect("the rows are published");
    assert!(
        !published[0].command.contains(secret),
        "{:?}",
        published[0].command
    );
    assert!(
        !published[0].output.contains(secret),
        "{:?}",
        published[0].output
    );
    assert!(published[0].command.starts_with("curl"));
}
