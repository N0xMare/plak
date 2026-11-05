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
/// so the next rows go out even when they are the same.
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
}
