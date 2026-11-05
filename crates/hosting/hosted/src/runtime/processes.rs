//! The session's process rows as its faces receive them: published only
//! when they change, and redacted as a tool row is.

use plakous_host_protocol::ProtocolExtension;
use plakous_host_protocol::views::ProcessView;

use super::Sender;
use crate::jobs::display;
use crate::surface_protocol::ContentEvent;

impl<P: ProtocolExtension> Sender<P> {
    /// Publishes the session's process rows when they changed since the
    /// last published ones: anything but a running process's elapsed time
    /// within the same second. A session that never had a process publishes
    /// none: no rows is what a face holds before the first. Rows go out
    /// redacted as a tool row's are.
    pub(crate) fn publish_processes(&self, views: Vec<ProcessView>) {
        let views: Vec<_> = views.into_iter().map(redacted_process).collect();
        let mut last = self
            .processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unchanged = match last.as_ref() {
            Some(held) => same_processes(held, &views),
            None => views.is_empty(),
        };
        if unchanged {
            return;
        }
        *last = Some(views.clone());
        drop(last);
        let running = views.iter().filter(|row| row.status.is_live()).count();
        let _ = self.send(ContentEvent::Processes { views, running });
    }
}

/// Whether two process lists show the same: a running process's elapsed
/// time counts only in whole seconds.
fn same_processes(held: &[ProcessView], next: &[ProcessView]) -> bool {
    held.len() == next.len()
        && held.iter().zip(next).all(|(old, new)| {
            old.id == new.id
                && old.command == new.command
                && old.owner == new.owner
                && old.started_at == new.started_at
                && old.status == new.status
                && old.output == new.output
                && old.elapsed_ms / 1000 == new.elapsed_ms / 1000
        })
}

/// A process row as every attachment, a remote one included, receives it
/// (spec 04 §2.10): its command redacted
/// ([`display::redacted_job_command`]) and bounded as a tool row's subject
/// is, and its output tail redacted as a whole-line tail
/// ([`display::redacted_job_tail`]).
fn redacted_process(mut view: ProcessView) -> ProcessView {
    view.command =
        crate::sessions::live_tool_subject(&display::redacted_job_command(&view.command));
    view.output = display::redacted_job_tail(&view.output);
    view
}
