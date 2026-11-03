//! The intent x role table (HOST v2 section 3.3), pinned against today's
//! behavior before M merges the child path onto the node actor.
//!
//! Every host intent belongs to one [`Row`]; every row has a root and a
//! child [`Column`]. A cell holds the classes the design asks for and the
//! classes today's code shows. One test per cell asserts today's class and
//! its observable outcome:
//!
//! * the root column, and the child cells that need a running host, are in
//!   `runtime/role_table_tests.rs`;
//! * the child cells that need a live child turn are in
//!   `agent/role_table_tests.rs`.
//!
//! Each test file lists its cells in a `COVERED` table, and
//! [`every_cell_has_exactly_one_test`] holds the union to one test per cell.
//! Where today differs from the design the cell's test carries an
//! `// M: expects <design class>` note, and [`DIVERGENT`] lists the cell.

#![allow(clippy::unwrap_used, reason = "role table tests")]

use crate::intent_dispatch::{DispatchPhase, Disposition, IntentClass};
use crate::runtime::test_kit::{ProbeIntent, TestExtension};
use plakous_host_protocol::views::transcript::TranscriptScope;

pub(crate) type UserAction = crate::surface_protocol::UserAction<TestExtension>;

/// How a loop takes an intent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Class {
    /// Urgent: taken at once, a running turn or workflow included.
    U,
    /// Idle-only: refused while a turn runs.
    I,
    /// Busy-handled: handled while a turn runs.
    B,
    /// Deferred, in arrival order, while a workflow runs.
    W,
    /// Refused: the role has no route for it.
    R,
}

/// One cell of the table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Cell {
    Classes(&'static [Class]),
    /// The child column of `Quit`: the host's stop detaches the child's
    /// attachment; stopping the child is a tree close or interrupt.
    Detach,
    /// Events only a child has, governed by the child role policy.
    ChildPolicy,
    /// The root has no such event.
    Absent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum Column {
    Root,
    Child,
}

/// The rows of HOST v2 section 3.3, in its order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum Row {
    /// `Prompt`, `FollowUp`.
    Prompt,
    /// `Steer`.
    Steer,
    /// `Interrupt`.
    Interrupt,
    /// `StopProcesses`, `Background`.
    Processes,
    /// `DropQueued`, `SendQueuedNow`, `DeleteQueued`, `EditQueued`,
    /// `MoveQueued`, `ResumeQueueDrain`.
    Queue,
    /// `ResolveApproval`.
    Approval,
    /// `Quit`.
    Quit,
    /// `NewSession`, `Compact`, `ForkSession`, `ResumeSession`,
    /// `ResumeSessionHandoff`. Switch and rebind arrive as product
    /// extension effects and are taken with the extension row.
    Workflow,
    /// `RefreshReadiness`, `RetryAttach`.
    Readiness,
    /// `Recover`.
    Recover,
    /// `OpenTopologyNavigator`, `RefreshTopologyTree`, `ResumeTopologyTree`,
    /// `InterruptTopologyNode`, `CloseTopologyNode`, `StopAllTopology`,
    /// `RecoverTopologyNode`.
    Navigator,
    /// `OpenAgentTranscript`, `CloseAgentTranscript`, `ResyncTranscript`.
    Transcript,
    /// `InvokeTool`.
    InvokeTool,
    /// `RequestCallDetail`.
    CallDetail,
    /// `RefreshTemplates`, `Extension`.
    Templates,
    /// Spawn, mail and wake Starts, nudges, transient resume,
    /// `submit_result`, result effects, close reservations, retirement,
    /// credential wait: no host intent carries them.
    ChildOnly,
}

impl Row {
    pub(crate) const ALL: [Self; 16] = [
        Self::Prompt,
        Self::Steer,
        Self::Interrupt,
        Self::Processes,
        Self::Queue,
        Self::Approval,
        Self::Quit,
        Self::Workflow,
        Self::Readiness,
        Self::Recover,
        Self::Navigator,
        Self::Transcript,
        Self::InvokeTool,
        Self::CallDetail,
        Self::Templates,
        Self::ChildOnly,
    ];
}

use Class::{B, I, R, U, W};

/// The design's cell.
pub(crate) const fn design(row: Row, column: Column) -> Cell {
    match (row, column) {
        (Row::Prompt, Column::Root) => Cell::Classes(&[I, B]),
        (Row::Workflow, Column::Root) => Cell::Classes(&[I, W]),
        (Row::Readiness | Row::Recover, Column::Root) => Cell::Classes(&[I]),
        (Row::Interrupt, _) | (Row::Quit, Column::Root) => Cell::Classes(&[U]),
        (Row::Quit, Column::Child) => Cell::Detach,
        (Row::ChildOnly, Column::Root) => Cell::Absent,
        (Row::ChildOnly, Column::Child) => Cell::ChildPolicy,
        (
            Row::Queue
            | Row::Workflow
            | Row::Readiness
            | Row::Recover
            | Row::Navigator
            | Row::InvokeTool,
            Column::Child,
        ) => Cell::Classes(&[R]),
        // Refused, except the usage extension, which is busy-handled.
        (Row::Templates, Column::Child) => Cell::Classes(&[R, B]),
        (
            Row::Prompt
            | Row::Steer
            | Row::Processes
            | Row::Queue
            | Row::Approval
            | Row::Navigator
            | Row::Transcript
            | Row::InvokeTool
            | Row::CallDetail
            | Row::Templates,
            _,
        ) => Cell::Classes(&[B]),
    }
}

/// Today's cell. A workflow defers every intent but `Interrupt`; `Quit`'s
/// deferral is its FIFO place, part of `U` for the root.
pub(crate) const fn today(row: Row, column: Column) -> Cell {
    match (row, column) {
        (Row::Prompt, Column::Root) => Cell::Classes(&[I, B, W]),
        (Row::Interrupt, _) | (Row::Quit, Column::Root) => Cell::Classes(&[U]),
        (Row::Quit, Column::Child) => Cell::Detach,
        (Row::Workflow | Row::Recover | Row::InvokeTool, Column::Root) => Cell::Classes(&[I, W]),
        // `RefreshReadiness` is ordinary, `RetryAttach` idle-only.
        (Row::Readiness, Column::Root) => Cell::Classes(&[I, B, W]),
        (Row::ChildOnly, Column::Root) => Cell::Absent,
        (Row::ChildOnly, Column::Child) => Cell::ChildPolicy,
        (
            Row::Steer
            | Row::Processes
            | Row::Queue
            | Row::Approval
            | Row::Navigator
            | Row::Transcript
            | Row::CallDetail
            | Row::Templates,
            Column::Root,
        ) => Cell::Classes(&[B, W]),
        (Row::Prompt | Row::Steer | Row::Transcript | Row::CallDetail, Column::Child) => {
            Cell::Classes(&[B])
        }
        // A user stop reaches the child's jobs; `Background` has no child route.
        (Row::Processes, Column::Child) => Cell::Classes(&[B, R]),
        // The intent names a child as its caller and runs idle.
        (Row::InvokeTool, Column::Child) => Cell::Classes(&[I]),
        (
            Row::Queue
            | Row::Approval
            | Row::Workflow
            | Row::Readiness
            | Row::Recover
            | Row::Navigator
            | Row::Templates,
            Column::Child,
        ) => Cell::Classes(&[R]),
    }
}

/// Every cell where today differs from the design. M closes these.
/// The root's differ only by today's workflow deferral (`W`), except
/// `Readiness`, `Recover` and `InvokeTool`, whose idle-only set differs.
pub(crate) const DIVERGENT: [(Row, Column); 16] = [
    (Row::Prompt, Column::Root),
    (Row::Steer, Column::Root),
    (Row::Processes, Column::Root),
    (Row::Queue, Column::Root),
    (Row::Approval, Column::Root),
    (Row::Readiness, Column::Root),
    (Row::Recover, Column::Root),
    (Row::Navigator, Column::Root),
    (Row::Transcript, Column::Root),
    (Row::InvokeTool, Column::Root),
    (Row::CallDetail, Column::Root),
    (Row::Templates, Column::Root),
    (Row::Processes, Column::Child),
    (Row::Approval, Column::Child),
    (Row::InvokeTool, Column::Child),
    (Row::Templates, Column::Child),
];

/// The row an intent belongs to. Exhaustive: a new intent must be placed.
pub(crate) fn row_of(action: &UserAction) -> Row {
    match action {
        UserAction::Prompt(_) | UserAction::FollowUp(_) => Row::Prompt,
        UserAction::Steer(_) => Row::Steer,
        UserAction::Interrupt => Row::Interrupt,
        UserAction::StopProcesses { .. } | UserAction::Background => Row::Processes,
        UserAction::DropQueued
        | UserAction::SendQueuedNow { .. }
        | UserAction::DeleteQueued { .. }
        | UserAction::EditQueued { .. }
        | UserAction::MoveQueued { .. }
        | UserAction::ResumeQueueDrain => Row::Queue,
        UserAction::ResolveApproval { .. } => Row::Approval,
        UserAction::Quit => Row::Quit,
        UserAction::NewSession
        | UserAction::Compact
        | UserAction::ForkSession { .. }
        | UserAction::ResumeSession { .. }
        | UserAction::ResumeSessionHandoff { .. } => Row::Workflow,
        UserAction::RefreshReadiness | UserAction::RetryAttach => Row::Readiness,
        UserAction::Recover => Row::Recover,
        UserAction::OpenTopologyNavigator
        | UserAction::RefreshTopologyTree
        | UserAction::ResumeTopologyTree
        | UserAction::InterruptTopologyNode { .. }
        | UserAction::CloseTopologyNode { .. }
        | UserAction::StopAllTopology
        | UserAction::RecoverTopologyNode { .. } => Row::Navigator,
        UserAction::OpenAgentTranscript { .. }
        | UserAction::CloseAgentTranscript
        | UserAction::ResyncTranscript { .. } => Row::Transcript,
        UserAction::InvokeTool { .. } => Row::InvokeTool,
        UserAction::RequestCallDetail { .. } => Row::CallDetail,
        UserAction::RefreshTemplates | UserAction::Extension(_) => Row::Templates,
    }
}

/// The child an intent addresses, when it can name one at all. An intent
/// that cannot is applied to the root session: a child face has no route
/// for it.
pub(crate) fn child_address(action: &UserAction) -> Option<&str> {
    match action {
        UserAction::OpenAgentTranscript { agent_id }
        | UserAction::InterruptTopologyNode { agent_id }
        | UserAction::CloseTopologyNode { agent_id }
        | UserAction::RecoverTopologyNode { agent_id, .. } => Some(agent_id),
        UserAction::InvokeTool { agent_id, .. }
        | UserAction::RequestCallDetail { agent_id, .. } => agent_id.as_deref(),
        UserAction::ResyncTranscript {
            scope: TranscriptScope::Agent(agent_id),
        } => Some(agent_id),
        _ => None,
    }
}

/// The number of intent variants [`row_of`] places.
const INTENT_VARIANTS: usize = 36;

/// One intent of every variant, child-addressed wherever the intent can be.
pub(crate) fn every_intent() -> Vec<UserAction> {
    let id = || "c1".to_owned();
    vec![
        UserAction::Prompt("p".into()),
        UserAction::FollowUp("f".into()),
        UserAction::Steer("s".into()),
        UserAction::Background,
        UserAction::Interrupt,
        UserAction::StopProcesses {
            id: None,
            interrupt: false,
        },
        UserAction::DropQueued,
        UserAction::SendQueuedNow {
            id: id(),
            revision: 1,
        },
        UserAction::DeleteQueued {
            id: id(),
            revision: 1,
        },
        UserAction::EditQueued {
            id: id(),
            revision: 1,
            text: id(),
        },
        UserAction::MoveQueued {
            id: id(),
            revision: 1,
            new_index: 0,
        },
        UserAction::ResumeQueueDrain,
        UserAction::ResolveApproval {
            approval_id: id(),
            choice: plakous_host_protocol::ApprovalChoice::Allow,
        },
        UserAction::Quit,
        UserAction::NewSession,
        UserAction::Compact,
        UserAction::ResumeSession { id: None },
        UserAction::ResumeSessionHandoff { id: id() },
        UserAction::ForkSession {
            account: None,
            compact: false,
        },
        UserAction::RefreshReadiness,
        UserAction::RetryAttach,
        UserAction::Recover,
        UserAction::OpenTopologyNavigator,
        UserAction::OpenAgentTranscript { agent_id: id() },
        UserAction::CloseAgentTranscript,
        UserAction::ResyncTranscript {
            scope: TranscriptScope::Agent(id()),
        },
        UserAction::RefreshTopologyTree,
        UserAction::ResumeTopologyTree,
        UserAction::InterruptTopologyNode { agent_id: id() },
        UserAction::CloseTopologyNode { agent_id: id() },
        UserAction::StopAllTopology,
        UserAction::RecoverTopologyNode {
            tree_id: id(),
            revision: 1,
            agent_id: id(),
            expected_attempt_id: id(),
        },
        UserAction::RefreshTemplates,
        UserAction::InvokeTool {
            correlation_id: None,
            name: "list_agents".to_owned(),
            arguments: "{}".to_owned(),
            agent_id: Some(id()),
        },
        UserAction::RequestCallDetail {
            call_id: id(),
            agent_id: Some(id()),
            session_id: id(),
        },
        UserAction::Extension(ProbeIntent::InspectUsage),
    ]
}

/// The row's intents, from [`every_intent`].
pub(crate) fn intents_of(row: Row) -> Vec<UserAction> {
    every_intent()
        .into_iter()
        .filter(|action| row_of(action) == row)
        .collect()
}

/// One cell a test file covers.
pub(crate) struct Covered {
    pub(crate) row: Row,
    pub(crate) column: Column,
    pub(crate) test: fn(),
}

/// The class a cell's test asserts it observed, checked against [`today`].
pub(crate) fn assert_today(row: Row, column: Column, observed: Cell) {
    assert_eq!(
        today(row, column),
        observed,
        "{row:?} x {column:?}: the test observed another class than the table pins"
    );
}

#[test]
fn every_intent_belongs_to_one_row() {
    let intents = every_intent();
    let variants = intents
        .iter()
        .map(std::mem::discriminant)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(variants.len(), intents.len(), "one intent per variant");
    assert_eq!(variants.len(), INTENT_VARIANTS, "every variant is listed");
    for row in Row::ALL {
        let placed = intents_of(row);
        assert_eq!(
            placed.is_empty(),
            row == Row::ChildOnly,
            "{row:?}: only the child-only row has no host intent"
        );
    }
}

/// Today's root letters are the dispatch table's: ordinary is `B`,
/// idle-only `I`, interrupt and quit `U`, and a workflow defers the rest.
#[test]
fn todays_root_column_is_the_dispatch_table() {
    for row in Row::ALL {
        let intents = intents_of(row);
        let Cell::Classes(classes) = today(row, Column::Root) else {
            assert_eq!(row, Row::ChildOnly);
            continue;
        };
        let mut derived = Vec::new();
        for action in &intents {
            let class = IntentClass::of(action);
            let letter = match class {
                IntentClass::Urgent | IntentClass::Quit => U,
                IntentClass::IdleOnly { .. } => I,
                IntentClass::Ordinary => B,
            };
            derived.push(letter);
            match class.disposition(DispatchPhase::Workflow) {
                Disposition::Defer if class != IntentClass::Quit => derived.push(W),
                Disposition::Defer | Disposition::ObserveInterrupt => {}
                other => panic!("{action:?}: a workflow does not {other:?}"),
            }
            // An ordinary prompt starts a turn when idle.
            if matches!(action, UserAction::Prompt(_) | UserAction::FollowUp(_)) {
                derived.push(I);
            }
        }
        for letter in classes {
            assert!(
                derived.contains(letter),
                "{row:?}: no intent shows {letter:?}"
            );
        }
        for letter in &derived {
            assert!(
                classes.contains(letter),
                "{row:?}: the table omits {letter:?}"
            );
        }
    }
}

#[test]
fn the_divergent_cells_are_exactly_where_today_differs_from_the_design() {
    for row in Row::ALL {
        for column in [Column::Root, Column::Child] {
            let differs = design(row, column) != today(row, column);
            assert_eq!(
                differs,
                DIVERGENT.contains(&(row, column)),
                "{row:?} x {column:?}: design {:?}, today {:?}",
                design(row, column),
                today(row, column)
            );
        }
    }
}

#[test]
fn every_cell_has_exactly_one_test() {
    let covered = crate::runtime::role_table_tests::COVERED
        .iter()
        .chain(crate::agent::role_table_tests::COVERED);
    let mut cells = std::collections::HashMap::new();
    let mut tests: Vec<fn()> = Vec::new();
    for cell in covered {
        tests.push(cell.test);
        *cells.entry((cell.row, cell.column)).or_insert(0_u32) += 1;
    }
    assert_eq!(tests.len(), Row::ALL.len() * 2, "one test per cell");
    for row in Row::ALL {
        for column in [Column::Root, Column::Child] {
            assert_eq!(
                cells.get(&(row, column)).copied(),
                Some(1),
                "{row:?} x {column:?} needs exactly one test"
            );
        }
    }
    assert_eq!(cells.len(), Row::ALL.len() * 2);
}
