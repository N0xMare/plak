//! Child-session lifecycle for a topology: binding a spawned child to a real
//! session, starting and reattaching its turns, the turn waiter, and reopening
//! a child after a restart. Implements the topology crate's binder from above.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use plakous::agent::{
    CommandId, RecoveredTurnHandle, TurnResult, TurnResultHandle, TurnResultOutcome,
};
use plakous::model::{ModelId, OutputBoundDisposition, TurnId};
use plakous::sessions::{
    AgentSessionsError, AgentSessionsErrorKind, CreateSessionOutcome, CreateSessionRequest,
    OpenSessionOutcome, OpenSessionRequest, RecoverEffectEvidenceOutcome,
    RecoverEffectEvidenceRequest, RecoveryRequiredReport, SessionEffectEvidence, SessionLocator,
    SessionOperationId, SessionRecoveryAction, SessionRuntimeBinding,
};
use plakous::subscriptions::NativeSubscriptionError;
use plakous::tools::{ToolExecutorId, ToolOutcome};

use crate::agent_tree::SubmitNudgeBudget;
use crate::agent_tree::turn_outcome::TurnOutcome;
use crate::agent_tree_tools::{SpawnBindError, TreeDriver};
use crate::binding::CompositionSelection;
use crate::child_prompts::ChildAssignmentStartKind;
use crate::prompt_inputs::{prompt_agent_tier, prompt_composition};
use crate::topology_runtime::{ChildStartup, ChildStartupStage, LiveRelease};

mod bind;
mod controller;
mod deliver;
mod failed_worktree;
mod reopen;
mod turns;
mod waiter;

#[cfg(test)]
pub(super) use reopen_error_tests::REOPEN_OPERATIONS;

pub(crate) mod runtime_factory;
pub use bind::{ChildBoundBind, spawn_child_start_composition, spawned_child_spec};
pub use turns::{persist_or_fault, retire_live_child, settle_or_fault};
#[cfg(any(test, feature = "test-support"))]
pub use waiter::finalize_topology_child_turn;

pub(crate) use bind::assignment_start_prompt;
pub(crate) use reopen::incompatible_session_message;

pub(super) use bind::{
    ProductChildSessionBinder, child_session_runtime_binding, spawn_child_operation_id,
};
#[cfg(test)]
pub(super) use bind::{
    child_plan_state_authority, child_session_workspace_root, create_spawned_child_session,
    frozen_child_tool_selection, frozen_openrouter_child_pin, openrouter_child_runtime_binding,
    openrouter_frozen_pin,
};
pub(super) use controller::ProductChildController;
#[cfg(test)]
pub(super) use deliver::turn_ended_or;
pub(super) use deliver::{ProductLiveCanceller, ProductMailDeliverer, submit_harness_prompt};
pub(super) use reopen::{
    CHILD_LEASE_POLL, CHILD_LEASE_WAIT, ChildReopen, ChildReopenError, ChildReopenErrorKind,
    ChildReopenMode, format_session_reopen_error, recover_owed_children, rejoin_recovered_start,
    reopen_spawned_child,
};
#[cfg(test)]
pub(super) use reopen::{SESSION_LEASE_HOLD_HINT, reconcile_reopened_results};
pub(super) use turns::{
    CHILD_COMMAND_CONSUMED, ChildStartFailure, ChildTurnIdentity, StartAdmission, end_attempt_work,
    mutate_child, release_startup, start_child_turn,
};
#[cfg(test)]
pub(super) use turns::{ChildCommand, start_child_turn_owned_with_ack};
#[cfg(test)]
use waiter::finalize_child_turn;
#[cfg(test)]
pub(super) use waiter::read_finalize_terminal;
pub(super) use waiter::{
    ChildFeedSource, FailedTurnWaits, TurnWaiterRegistration, finalize_child_failure, turn_watch,
};
#[cfg(test)]
pub(super) use waiter::{child_turn_was_cancelled, start_instruction_turn, transient_resume_bound};

use bind::{SPAWN_MATERIALIZE_TIMEOUT, StartupCaller};
use deliver::mail_command_id;
use reopen::{bound_child_turn_matches, occupy_recovered_child};
use turns::start_child_turn_owned;
use waiter::register_admitted_turn;

use super::{
    ApiTopologyParent, AttachedCodingSession, ProductAgentSpec, SessionToolFence, SpawnParent,
    coding_session_runtime_binding_for, frozen_parent_composition, occupancy_session_fence,
    product_subscription_request_limits, require_reconstructed_coding_projection,
    selected_coding_provider_configuration_with_processing_tier, spawn_child_composition,
    spawn_parent_tree_id, spawn_roster_composition, spawn_roster_processing_tier,
};

#[cfg(test)]
mod reopen_error_tests;
#[cfg(test)]
mod retirement_tests;
#[cfg(test)]
mod startup_owner_tests;
