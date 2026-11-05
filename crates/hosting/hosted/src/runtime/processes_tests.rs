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

/// Secrets a heredoc writes on lines of their own, as the runtime displays
/// the command: each newline escaped as `\n`. Each pair is the line and the
/// part of it that must not be shown.
const HEREDOC_SECRETS: [(&str, &str); 4] = [
    (
        "ghp_abcdefghijklmnopqrstuvwxyz0123",
        "abcdefghijklmnopqrstuvwxyz0123",
    ),
    (
        "sk-abcdefghijklmnopqrstuvwxyz0123",
        "abcdefghijklmnopqrstuvwxyz0123",
    ),
    ("AKIAABCDEFGHIJKLMNOP", "ABCDEFGHIJKLMNOP"),
    (
        "Authorization: Bearer abcdefghijklmnopqrstuvwxyz",
        "abcdefghijklmnopqrstuvwxyz",
    ),
];

/// A key the runtime's 1 KiB cut left without its END.
const CUT_KEY: &str = "cat > id <<EOF\\n-----BEGIN OPENSSH PRIVATE KEY-----\\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQ";

/// The command of a published row as the runtime displays it: newlines
/// escaped, cut. A token on its own line and a key without its END are
/// redacted all the same, and the escapes stay as they were displayed.
#[test]
fn a_heredoc_secret_in_a_displayed_command_is_redacted() {
    use crate::jobs::display::redacted_job_command;
    for (line, secret) in HEREDOC_SECRETS {
        let command = format!("gh auth login --with-token <<EOF\\n{line}\\nEOF");
        let shown = redacted_job_command(&command);
        assert!(!shown.contains(secret), "{shown:?}");
        assert!(
            shown.starts_with("gh auth login --with-token <<EOF\\n") && shown.ends_with("\\nEOF"),
            "{shown:?}"
        );
    }
    let shown = redacted_job_command(CUT_KEY);
    assert_eq!(shown, "cat > id <<EOF\\n[redacted]");
    let plain = "printf 'a\\nb' && make\\r\\ntest";
    assert_eq!(redacted_job_command(plain), plain);
}

/// The runtime starts a tail at a line, or marks one cut inside its only
/// line; that fragment is not shown, only the mark. A key whose BEGIN was
/// cut off is redacted up to its END.
#[test]
fn a_tail_shows_whole_lines_and_no_orphan_key() {
    use crate::jobs::display::redacted_job_tail;
    let fragment = plakous::tools::JOB_TAIL_FRAGMENT;
    assert_eq!(
        redacted_job_tail(&format!("{fragment}cdefghijklmnopqrstuvwxyz0123456789")),
        fragment
    );
    assert_eq!(redacted_job_tail("first\nsecond"), "first\nsecond");
    assert_eq!(
        redacted_job_tail("Zm9vYmFyYmF6cXV4\nMIIEvQIBADANBg\n-----END RSA PRIVATE KEY-----\nafter"),
        "[redacted]\nafter"
    );
    assert_eq!(
        redacted_job_tail("-----END CERTIFICATE-----\nkept"),
        "-----END CERTIFICATE-----\nkept"
    );
}
