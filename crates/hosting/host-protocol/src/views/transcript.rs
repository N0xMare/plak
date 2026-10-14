//! Typed transcript rows: the one row vocabulary the host persists in the
//! product transcript, carries in the export document, emits live and
//! restores.
//!
//! A live row and the row a restore reads back are the same value, so a
//! renderer that maps rows to pixels shows a resumed session exactly like the
//! live one. Rows are presentation only: canonical history stays owned by
//! Plakous, and nothing here is checkpoint or recovery authority.
//!
//! Grouping (spec 02 §5.4): a group is a run of tool items inside one root
//! turn. [`TranscriptRow::ends_group`] names the rows that close it; every
//! other row is an item inside the current group.

use serde::{Deserialize, Serialize};

use super::presentation::ActivityState;

/// One transcript row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptRow {
    /// Muted first row: the session id and, for a carried session, its source.
    SessionHeader {
        /// This product session.
        session_id: String,
        /// The source session an import carried this one from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        carried_from: Option<CarriedFrom>,
    },
    /// An admitted user prompt.
    User {
        /// Prompt text as the user wrote it.
        text: String,
        /// When the operator sent it, Unix milliseconds (host-stamped).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sent_at_ms: Option<u64>,
    },
    /// A steer delivered into a running turn.
    Steer {
        /// Steer text as the user wrote it.
        text: String,
        /// When the operator sent it, Unix milliseconds (host-stamped).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sent_at_ms: Option<u64>,
    },
    /// Published assistant prose of one turn.
    Assistant {
        /// Markdown source.
        text: String,
    },
    /// One sealed, operator-visible reasoning block.
    Thinking(ThinkingRow),
    /// One tool call. Compaction is the tool row named `compact`.
    Tool(ToolItem),
    /// A background process the turn started exited or was stopped.
    Process(ProcessNotice),
    /// The turn's model request is being sent again.
    Retry(RetryItem),
    /// A lifecycle event of a child agent.
    Milestone(AgentMilestone),
    /// The host woke the root for a batch of settled children and processes.
    ///
    /// No longer written; one a session stored earlier is kept and not shown.
    AutoContinue(AutoContinue),
    /// A classified operator notice; see [`NoticeScope`].
    Notice(NoticeRow),
    /// The turn was interrupted before it published a result.
    Interrupted,
    /// Older rows were dropped by the product transcript bound. Only ever the
    /// first row.
    Truncated,
}

impl TranscriptRow {
    /// A user row without a send time (a child's or a history-derived row).
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            text: text.into(),
            sent_at_ms: None,
        }
    }

    /// A steer row without a send time.
    #[must_use]
    pub fn steer(text: impl Into<String>) -> Self {
        Self::Steer {
            text: text.into(),
            sent_at_ms: None,
        }
    }

    /// An assistant row.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant { text: text.into() }
    }

    /// A notice row in `scope` with an informational tone.
    #[must_use]
    pub fn info(scope: NoticeScope, text: impl Into<String>) -> Self {
        Self::notice(scope, NoticeTone::Info, text)
    }

    /// A notice row in `scope` with a warning tone.
    #[must_use]
    pub fn warning(scope: NoticeScope, text: impl Into<String>) -> Self {
        Self::notice(scope, NoticeTone::Warning, text)
    }

    /// An error row in `scope`.
    #[must_use]
    pub fn error(scope: NoticeScope, text: impl Into<String>) -> Self {
        Self::notice(scope, NoticeTone::Error, text)
    }

    fn notice(scope: NoticeScope, tone: NoticeTone, text: impl Into<String>) -> Self {
        Self::Notice(NoticeRow {
            scope,
            tone,
            text: text.into(),
        })
    }

    /// Whether this row closes the current tool group (spec 02 §5.4). Tool,
    /// process, retry and item-scoped notices stay inside it.
    #[must_use]
    pub fn ends_group(&self) -> bool {
        match self {
            Self::Tool(tool) => tool.name == "compact",
            Self::Process(_) | Self::Retry(_) | Self::Truncated | Self::AutoContinue(_) => false,
            Self::Notice(notice) => {
                notice.tone == NoticeTone::Error || notice.placement() == NoticePlacement::NoticeRow
            }
            Self::SessionHeader { .. }
            | Self::User { .. }
            | Self::Steer { .. }
            | Self::Assistant { .. }
            | Self::Thinking(_)
            | Self::Milestone(_)
            | Self::Interrupted => true,
        }
    }

    /// The tool call id this row is addressed by, for in-place updates.
    #[must_use]
    pub fn call_id(&self) -> Option<&str> {
        match self {
            Self::Tool(tool) => Some(&tool.call_id),
            _ => None,
        }
    }

    /// The operator text of a prose, thinking or notice row.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::User { text, .. } | Self::Steer { text, .. } | Self::Assistant { text } => {
                Some(text)
            }
            Self::Thinking(thinking) => Some(&thinking.text),
            Self::Notice(notice) => Some(&notice.text),
            _ => None,
        }
    }

    /// The tool item this row carries.
    #[must_use]
    pub fn as_tool(&self) -> Option<&ToolItem> {
        match self {
            Self::Tool(tool) => Some(tool),
            _ => None,
        }
    }

    /// The thinking row this row carries.
    #[must_use]
    pub fn as_thinking(&self) -> Option<&ThinkingRow> {
        match self {
            Self::Thinking(thinking) => Some(thinking),
            _ => None,
        }
    }

    /// Whether this row is an error the operator must see even when an
    /// observation queue coalesces.
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Notice(notice) if notice.tone == NoticeTone::Error)
    }
}

/// The source of a carried session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CarriedFrom {
    /// Source product session id, under the source state key.
    pub session_id: String,
    /// State key of the build the session was exported from.
    pub state_key: String,
}

/// A sealed thinking row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkingRow {
    /// Opaque per-turn reasoning location.
    pub location_key: String,
    /// Already-redacted, bounded reasoning text.
    pub text: String,
    /// Host-observed reasoning duration once sealed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// One tool call as the transcript shows it. Written in place by call id as
/// the call progresses; the host sets its activity kind, which the renderer
/// never re-derives from `name`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolItem {
    /// Stable call id.
    pub call_id: String,
    /// Canonical tool name; the activity kind derives from it.
    pub name: String,
    /// The exact name the model called, present only when it differs from
    /// `name`. The row is labelled by it (chapter 01 §8.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposed_name: Option<String>,
    /// Activity category, decided by the host from the canonical name.
    /// Serialized as `activity_kind`: `kind` is the row's own tag.
    #[serde(rename = "activity_kind")]
    pub kind: super::presentation::ActivityKind,
    /// Lifecycle label: `running`, `done[:elapsed]`, `failed[:reason]`,
    /// `rejected…`, `cancelled`, `interrupted`, `overflow`.
    pub state: String,
    /// One-line subject (command, path, query, …), bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Bounded expandable detail (arguments and result preview).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Host start stamp, Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    /// Host finish stamp, Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    /// Outer local-composition marker call owning this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_call_id: Option<String>,
    /// Local-composition cell owning this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    /// Facts of a wait call: `wait_agent` or a process wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitFacts>,
}

impl ToolItem {
    /// The name the row is labelled by: the exposed name when the model
    /// called the tool by another name, the canonical name otherwise.
    #[must_use]
    pub fn label(&self) -> &str {
        self.exposed_name.as_deref().unwrap_or(&self.name)
    }

    /// The lifecycle state the persisted `state` label names.
    #[must_use]
    pub fn activity_state(&self) -> ActivityState {
        match self.state.as_str() {
            "running" => ActivityState::Running,
            "succeeded" | "done" => ActivityState::Succeeded,
            state if state.starts_with("done:") => ActivityState::Succeeded,
            state
                if matches!(
                    state.split_whitespace().next(),
                    Some("interrupted" | "cancelled" | "canceled")
                ) =>
            {
                ActivityState::Cancelled
            }
            "overflow" => ActivityState::Failed {
                reason: Some("overflow".to_owned()),
            },
            failure => ActivityState::Failed {
                reason: failure.strip_prefix("failed:").map(str::to_owned),
            },
        }
    }

    /// Elapsed time from the host's own start and finish stamps.
    #[must_use]
    pub fn duration_ms(&self) -> Option<u64> {
        self.started_at_ms
            .zip(self.finished_at_ms)
            .and_then(|(start, finish)| finish.checked_sub(start))
    }
}

/// What a wait asked for and what ended it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaitFacts {
    /// Requested timeout, when the call named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// What woke it, e.g. `c39 finished` or `timeout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub woke_by: Option<String>,
    /// Host-observed actual wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waited_ms: Option<u64>,
    /// The agent whose settlement ended the wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_agent: Option<String>,
}

/// A background process notice, an item inside the group.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessNotice {
    /// Product job id.
    pub job_id: String,
    /// How the process ended.
    pub outcome: ProcessOutcome,
    /// Host-observed run time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The process's command: one redacted line, bounded like a tool subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// How a background process ended.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessOutcome {
    /// The process exited on its own, with its code when known.
    Exited {
        /// Exit code, absent for a signal or an unknown status.
        code: Option<i32>,
    },
    /// The process was stopped.
    Stopped,
}

/// One announced model-request replay: `retry 2/4 · cause`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetryItem {
    /// The 1-based physical request about to be sent.
    pub attempt: u32,
    /// The physical-request budget.
    pub max_attempts: u32,
    /// Operator-facing cause.
    pub cause: String,
}

/// A child-agent lifecycle row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMilestone {
    /// Tree-scoped agent id, e.g. `c23`.
    pub agent_id: String,
    /// What happened.
    pub event: MilestoneEvent,
    /// Child template, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Brief, result, failure reason or question; the row shows its first
    /// line and the detail shows all of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Attempt run time, for terminal events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Candidate id, for adoption events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    /// Candidate file count, for an adoption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    /// Conflicting paths, for an adopt conflict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// Child lifecycle events that render as rows.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MilestoneEvent {
    /// `◆ spawned c23 · template · brief`.
    Spawned,
    /// `◇ re-tasked c23 · brief`: a follow-up started a new attempt.
    Retasked,
    /// `✓ c23 finished · 14m`.
    Finished,
    /// `✗ c23 failed · reason`.
    Failed,
    /// `⚠ c23 needs recovery`.
    NeedsRecovery,
    /// `? c23 asks · question`.
    Asks,
    /// `⇲ adopted cand-c23-a1 · 16 files`.
    Adopted,
    /// `✗ adopt conflict · path`.
    AdoptConflict,
}

/// A wake batch an earlier build recorded; never shown.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutoContinue {
    /// Children whose settled results woke the root.
    pub agents_finished: u32,
    /// Background processes whose exits woke the root.
    pub processes_exited: u32,
    /// The settled agent ids, in wake order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
}

/// A classified operator notice.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoticeRow {
    /// What the notice is about; decides where it renders.
    pub scope: NoticeScope,
    /// Severity; an error renders as an error row.
    pub tone: NoticeTone,
    /// Operator-facing text.
    pub text: String,
}

impl NoticeRow {
    /// Where a notice in this row's scope renders.
    #[must_use]
    pub const fn placement(&self) -> NoticePlacement {
        self.scope.placement()
    }
}

/// Every host and product notice producer names one scope. The scope alone
/// decides placement, so a new producer is classified when it is written.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeScope {
    /// A model-request retry cause or output-limit retry inside a turn.
    Retry,
    /// A tool observation was clipped before the model saw it.
    Clipped,
    /// Turn outcome guidance: empty stream, reasoning-only, budget exhausted,
    /// context limit, definite failure, refusal.
    Turn,
    /// Queue and delivery state: a prompt remains queued, already ran, send-now.
    Queue,
    /// Recovery: pending recovery, abandoned work recovered, uncertain state.
    Recovery,
    /// Agent tree: paused tree or auto-continue, stopped children, unattended
    /// run bounds, navigator and `invoke_tool` outcomes.
    Tree,
    /// Compaction outcomes and refusals.
    Compaction,
    /// Session lifecycle: attach, fork, clear, resume, reload.
    Session,
    /// Binding and settings feedback: model, account, profile, config.
    Settings,
    /// Approval decisions that could not be applied.
    Approval,
    /// Host faults: a commit or channel failure the operator must know about.
    Host,
    /// Internal diagnostics. Session log only; never rendered.
    Diagnostic,
}

impl NoticeScope {
    /// The classification table (spec 02 §5.4).
    #[must_use]
    pub const fn placement(self) -> NoticePlacement {
        match self {
            Self::Retry | Self::Clipped => NoticePlacement::GroupItem,
            Self::Turn
            | Self::Queue
            | Self::Recovery
            | Self::Tree
            | Self::Compaction
            | Self::Session
            | Self::Settings
            | Self::Approval
            | Self::Host => NoticePlacement::NoticeRow,
            Self::Diagnostic => NoticePlacement::SessionLog,
        }
    }
}

impl NoticeScope {
    /// Whether a host notice in this scope is part of the session's
    /// transcript (spec 02 §5.4): one that describes the conversation (turn
    /// outcomes, retries, queue and delivery, recovery, the agent tree,
    /// compaction, session lifecycle, host faults) persists with the session
    /// and restores exactly as it was shown live. Feedback about the
    /// operator's own settings or approvals and the clipped-observation item
    /// are live only; a diagnostic is never shown at all.
    #[must_use]
    pub const fn persisted(self) -> bool {
        match self {
            Self::Retry
            | Self::Turn
            | Self::Queue
            | Self::Recovery
            | Self::Tree
            | Self::Compaction
            | Self::Session
            | Self::Host => true,
            Self::Clipped | Self::Settings | Self::Approval | Self::Diagnostic => false,
        }
    }
}

/// Where a notice renders.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoticePlacement {
    /// An item inside the current tool group.
    GroupItem,
    /// A gutter notice row that ends the group.
    NoticeRow,
    /// Written to the session log only.
    SessionLog,
}

/// Notice severity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeTone {
    /// Informational.
    Info,
    /// Needs attention.
    Warning,
    /// An error row.
    Error,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
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
}
