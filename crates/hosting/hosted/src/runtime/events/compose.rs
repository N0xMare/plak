//! Compose cell rows and observed counts.

use super::tools::observe_tool_activity;
use crate::runtime::{ComposeProjection, Sender};
use crate::session_log::redact_text_preview;
use crate::sessions;
use crate::sessions::SessionRecord;
use crate::surface_protocol::ContentEvent;

impl ComposeProjection {
    /// The row state to re-emit with a new tally.
    ///
    /// Spec 02 §5.4: "in flight, the running glyph appears on both the compose and
    /// the child that is running". An `exec*` that yielded with a `wait*`
    /// continuation has an observed terminal of its own, but children dispatched
    /// during the wait phase are still running beneath it — painting `✓` there
    /// would claim an outcome over children the host has not observed
    /// terminating (§5.1 "no invented state"), and would leave `✓ Compose` sitting
    /// over `⊘ cancelled` children after a cancel mid-`wait*`. The row is
    /// terminal only when the marker terminalised *and* every observed child is
    /// terminal; the `wait*` terminal that closes the last child re-terminalises
    /// it. Both halves are host-observed, never inferred from the program.
    pub(in crate::runtime) fn row_state(&self) -> &str {
        match self.terminal_state.as_deref() {
            Some(state) if self.all_observed_children_terminal() => state,
            _ => "running",
        }
    }

    /// The persisted state for a settled marker row, or `None` while the row
    /// is still in flight.
    ///
    /// The transcript vocabulary is `succeeded` / `failed`, not the row states
    /// [`ComposeProjection::row_state`] paints.
    pub(in crate::runtime) fn settled_persisted_state(&self) -> Option<&'static str> {
        match self.row_state() {
            "running" => None,
            "failed" => Some("failed"),
            _ => Some("succeeded"),
        }
    }

    /// Host-observed only: every child call id seen at all has also been seen
    /// reaching a terminal state.
    pub(in crate::runtime) fn all_observed_children_terminal(&self) -> bool {
        self.observed_children
            .iter()
            .all(|child| self.terminal_children.contains_key(child))
    }

    pub(in crate::runtime) fn child_count(&self) -> usize {
        self.observed_children.len()
    }

    pub(in crate::runtime) fn failed_count(&self) -> usize {
        self.terminal_children
            .values()
            .filter(|failed| **failed)
            .count()
    }

    pub(in crate::runtime) fn terminal_count(&self) -> usize {
        self.terminal_children.len()
    }
}

/// The program a Compose cell ran, as the operator wrote it (§5.8).
///
/// Codex `exec` arguments are raw UTF-8 program source, and the observed
/// preview carries them as one JSON *string* — rendering that form put a
/// leading quote and escaped newlines behind the rail. A Plak PTC marker
/// instead carries a JSON body whose `source` / `code` / `program` field holds
/// the program. Anything else is taken verbatim. Nothing is invented: a marker
/// without a program yields `None`.
pub(super) fn compose_program_source(arguments: Option<&str>) -> Option<String> {
    let arguments = arguments?.trim();
    if arguments.is_empty() {
        return None;
    }
    let source = match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(serde_json::Value::String(program)) => program,
        Ok(serde_json::Value::Object(body)) => body
            .get("source")
            .or_else(|| body.get("code"))
            .or_else(|| body.get("program"))
            .and_then(serde_json::Value::as_str)?
            .to_owned(),
        _ => arguments.to_owned(),
    };
    let source = source.trim_matches('\n');
    (!source.trim().is_empty()).then(|| source.to_owned())
}

/// Renders one Compose row's host-counted, host-observed tally.
///
/// These are counts over a droppable observation stream, never a claim about
/// what the cell executed and never a billing or capacity claim.
pub(super) fn compose_counts_label(compose: &ComposeProjection, gap_observed: bool) -> String {
    compose_counts_subject(compose.child_count(), compose.failed_count(), gap_observed)
}

pub(crate) fn compose_counts_subject(count: usize, failed: usize, gap_observed: bool) -> String {
    let mut label = format!("{count} tools");
    if failed > 0 {
        label.push_str(&format!(" · {failed} failed"));
    }
    if gap_observed {
        label.push_str(" · counts partial after gap");
    }
    label
}

/// Whether a row's subject is the host's own (web, image and MCP rows), so
/// the runtime's generic subject must not replace it.
pub(crate) fn host_owns_subject(canonical: &str, exposed: &str) -> bool {
    crate::runtime::tools::is_web_or_mcp_row(canonical)
        || crate::runtime::tools::is_web_or_mcp_row(exposed)
}

pub(crate) fn projected_tool_subject(name: &str, arguments: Option<&str>) -> Option<String> {
    let arguments = serde_json::from_str(arguments?).ok()?;
    crate::runtime::tools::tool_subject_from_args(name, &arguments)
        .map(|subject| sessions::bound_subject(&redact_text_preview(&subject)))
}

/// Terminal state text for a cancelled Compose row.
///
/// The remainder is only ever rendered from what the host observed: without an
/// observed ordinal there is no ratio to state.
pub(super) fn compose_cancelled_state(compose: &ComposeProjection) -> String {
    if compose.highest_ordinal == 0 {
        return "cancelled".to_owned();
    }
    format!(
        "cancelled after {} of {}",
        compose.terminal_count(),
        compose.highest_ordinal
    )
}

/// Re-persists a Compose head's counts after a child terminal moved them.
///
/// The head's own `ToolCompleted` persists the tally it had at that moment; a
/// child that terminalises later — during a `wait*` continuation, or a
/// rejected accepted request — is counted live by `emit_compose_update` but was
/// never written back, so a resumed transcript showed the stale `3 tools`
/// against the live `5 tools`.
pub(super) fn observe_compose_subject(
    session: &mut SessionRecord,
    marker_call: &str,
    subject: String,
) {
    let Some(mut activity) = session.transcript.tool_activity(marker_call) else {
        return;
    };
    if activity.subject.as_deref() == Some(subject.as_str()) {
        return;
    }
    activity.subject = Some(subject);
    observe_tool_activity(session, activity);
}

/// Re-persists a reopened Compose marker as terminal once its last observed
/// child terminalises.
///
/// A cell that yields and then dispatches during its `wait*` continuation is
/// written back to `running`, so a cancel in that window sweeps it. When the
/// wait phase instead closes cleanly nothing wrote the marker terminal again,
/// so the `Completed` sweep over persisted `running` rows rewrote a cell that
/// had succeeded to `failed:observation-gap` — live and on resume. The row is
/// re-persisted only from what the host observed: the
/// marker's own terminal plus every observed child terminalising.
pub(super) fn observe_settled_compose_marker(
    session: &mut SessionRecord,
    compose: &ComposeProjection,
    gap_observed: bool,
    marker_call: &str,
    finished_at_ms: u64,
) {
    let Some(state) = compose.settled_persisted_state() else {
        return;
    };
    let Some(mut marker) = session.transcript.tool_activity(marker_call) else {
        return;
    };
    let subject = compose_counts_label(compose, gap_observed);
    if marker.state == state
        && marker.finished_at_ms.is_some()
        && marker.subject.as_deref() == Some(subject.as_str())
    {
        return;
    }
    state.clone_into(&mut marker.state);
    marker.finished_at_ms = Some(finished_at_ms);
    marker.subject = Some(subject);
    observe_tool_activity(session, marker);
}

pub(super) fn emit_compose_update<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    session: &SessionRecord,
    compose: &ComposeProjection,
    call_id: &str,
    gap_observed: bool,
    duration_ms: Option<u64>,
) {
    // The cell's persisted start; a terminal head ends where its duration,
    // measured from that start, does.
    let started_at_ms = session
        .transcript
        .tool_activity(call_id)
        .and_then(|row| row.started_at_ms);
    let finished_at_ms = (compose.row_state() != "running")
        .then(|| started_at_ms.zip(duration_ms))
        .flatten()
        .map(|(started, elapsed)| started.saturating_add(elapsed));
    let _ = events.send(ContentEvent::ToolLine {
        call_id: Some(call_id.to_owned()),
        parent_call_id: None,
        cell_id: compose.cell_id.clone(),
        kind: plakous_host_protocol::views::presentation::ActivityKind::Compose,
        name: compose.name.clone(),
        state: compose.row_state().into(),
        subject: Some(compose_counts_label(compose, gap_observed)),
        detail: None,
        duration_ms,
        started_at_ms,
        finished_at_ms,
        wait: None,
    });
}

#[cfg(test)]
mod tests;
