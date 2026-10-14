//! The bounded task plan.
//!
//! A stateful port with its own identity and revision protocol: the
//! `PlanState` port, its in-memory implementation, the replacement tool, and
//! the id-merge `todo_write` lowering.

#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact provider tool identifiers"
)]

mod restore;
mod text;

pub use restore::plan_restoration_note;
pub use text::{
    PLAKOUS_CODEX_UPDATE_PLAN_DESCRIPTION, PLAKOUS_GROK_TODO_DESCRIPTION,
    PLAKOUS_MERGE_PLAN_DESCRIPTION, PLAKOUS_PLAN_RESTORED_NOTE_HEADER,
    PLAKOUS_TOOL_MERGE_PLAN_COLD_MAP,
};

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use plakous_model::{ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract};
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Value, json};

use plakous_tools::{
    CanonicalProcessOperationV2, CodingPresentation, PlanStateIdentity, PromptPin,
    RegisteredToolId, ToolExecutorBindingIdentity, ToolProjectionLoweringV2, object_schema,
};
use plakous_tools::{MenuBinding, MenuEntry, MenuShape, ToolPack, tool_name};
use plakous_tools::{
    ToolContext, ToolExecutor, ToolExecutorId, ToolFuture, ToolInvocation, ToolOutcome,
    ToolRegistration, ToolRuntimeError, ToolRuntimeErrorKind,
};

use plakous_tools::decode_arguments;
use plakous_tools::{
    MAX_STANDARD_PLAN_EXPLANATION_BYTES, MAX_STANDARD_PLAN_ITEMS, MAX_STANDARD_PLAN_STEP_BYTES,
    MERGE_PLAN_CAPABILITY_NAME, STANDARD_CODING_TOOL_VERSION, UPDATE_PLAN_CAPABILITY_NAME,
};
use plakous_tools::{
    PLAKOUS_TOOL_UPDATE_PLAN_DESCRIPTION, standard_registration_with_executor,
    update_plan_definition, update_plan_input_schema, workspace_concurrency,
};

/// One immutable plan item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanItem {
    step: String,
    status: PlanItemStatus,
}

impl PlanItem {
    /// Creates one bounded plan item.
    ///
    /// # Errors
    ///
    /// Returns a resource error for empty or oversized step text.
    pub fn new(step: impl Into<String>, status: PlanItemStatus) -> Result<Self, ToolRuntimeError> {
        let step = step.into();
        validate_plan_step(&step)?;
        Ok(Self { step, status })
    }

    /// Returns the exact model-visible step text.
    #[must_use]
    pub fn step(&self) -> &str {
        &self.step
    }

    /// Returns the item status.
    #[must_use]
    pub const fn status(&self) -> PlanItemStatus {
        self.status
    }
}

impl<'de> Deserialize<'de> for PlanItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SerializedPlanItem {
            step: String,
            status: PlanItemStatus,
        }

        let item = SerializedPlanItem::deserialize(deserializer)?;
        Self::new(item.step, item.status)
            .map_err(|_| de::Error::custom("plan step is empty or exceeds its UTF-8 byte limit"))
    }
}

fn validate_plan_step(step: &str) -> Result<(), ToolRuntimeError> {
    if step.is_empty() || step.len() > MAX_STANDARD_PLAN_STEP_BYTES {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::ResourceLimit,
            "plan step is empty or exceeds its byte limit",
        ));
    }
    Ok(())
}

/// Portable plan-item state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlanItemStatus {
    /// Work has not started.
    Pending,
    /// Work is currently active.
    InProgress,
    /// Work completed.
    Completed,
}

/// Immutable revisioned plan snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanSnapshot {
    revision: u64,
    explanation: Option<Arc<str>>,
    items: Arc<[PlanItem]>,
}

impl PlanSnapshot {
    /// Creates an authoritative non-empty plan snapshot.
    ///
    /// This constructor lets application-owned durable [`PlanState`]
    /// implementations return the same bounded value as the built-in state.
    /// Revision zero is reserved for [`Self::empty`].
    ///
    /// # Errors
    ///
    /// Returns an argument/resource error for revision zero or for the same
    /// invalid plan content rejected by [`PlanReplacement::new`].
    pub fn new(
        revision: u64,
        explanation: Option<String>,
        items: Vec<PlanItem>,
    ) -> Result<Self, ToolRuntimeError> {
        if revision == 0 {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "non-empty plan snapshots require a nonzero revision",
            ));
        }
        let replacement = PlanReplacement::new(None, explanation, items)?;
        Ok(Self {
            revision,
            explanation: replacement.explanation,
            items: replacement.items,
        })
    }

    /// Returns the canonical empty revision-zero plan snapshot.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            revision: 0,
            explanation: None,
            items: Arc::from([]),
        }
    }

    /// Returns the monotonic state revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the optional explanation.
    #[must_use]
    pub fn explanation(&self) -> Option<&str> {
        self.explanation.as_deref()
    }

    /// Returns ordered plan items.
    #[must_use]
    pub fn items(&self) -> &[PlanItem] {
        &self.items
    }
}

/// Validated complete replacement submitted to a plan-state owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanReplacement {
    expected_revision: Option<u64>,
    explanation: Option<Arc<str>>,
    items: Arc<[PlanItem]>,
}

impl PlanReplacement {
    /// Creates a bounded complete replacement.
    ///
    /// # Errors
    ///
    /// Returns an argument/resource error for an empty plan, excessive item
    /// count, oversized explanation, or more than one active item.
    pub fn new(
        expected_revision: Option<u64>,
        explanation: Option<String>,
        items: Vec<PlanItem>,
    ) -> Result<Self, ToolRuntimeError> {
        if items.is_empty() || items.len() > MAX_STANDARD_PLAN_ITEMS {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "plan must contain a bounded non-empty item list",
            ));
        }
        for item in &items {
            validate_plan_step(item.step())?;
        }
        if items
            .iter()
            .filter(|item| item.status() == PlanItemStatus::InProgress)
            .count()
            > 1
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "plan may contain at most one in-progress item",
            ));
        }
        let explanation = explanation
            .map(|value| {
                if value.is_empty() || value.len() > MAX_STANDARD_PLAN_EXPLANATION_BYTES {
                    Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::ResourceLimit,
                        "plan explanation is empty or exceeds its byte limit",
                    ))
                } else {
                    Ok(Arc::<str>::from(value))
                }
            })
            .transpose()?;
        Ok(Self {
            expected_revision,
            explanation,
            items: items.into(),
        })
    }

    /// Returns the optimistic expected revision, when supplied.
    #[must_use]
    pub const fn expected_revision(&self) -> Option<u64> {
        self.expected_revision
    }

    /// Returns replacement explanation.
    #[must_use]
    pub fn explanation(&self) -> Option<&str> {
        self.explanation.as_deref()
    }

    /// Returns replacement items.
    #[must_use]
    pub fn items(&self) -> &[PlanItem] {
        &self.items
    }
}

/// Owned future returned by a plan-state owner.
pub type PlanStateFuture =
    Pin<Box<dyn Future<Output = Result<PlanSnapshot, ToolRuntimeError>> + Send + 'static>>;

/// Application-owned revisioned plan-state port.
///
/// Durable products may implement this port over a session component. The
/// standard tool never treats its progress/event stream as authoritative state.
pub trait PlanState: Send + Sync + 'static {
    /// Returns the immutable concrete state-authority identity.
    fn identity(&self) -> PlanStateIdentity;

    /// Returns the current immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns a bounded state-owner error when the snapshot is unavailable.
    fn snapshot(&self) -> Result<PlanSnapshot, ToolRuntimeError>;

    /// Atomically validates and installs one complete replacement.
    fn replace(&self, replacement: PlanReplacement) -> PlanStateFuture;
}

/// Bounded process-local plan state for non-durable embeddings and tests.
pub struct InMemoryPlanState {
    identity: PlanStateIdentity,
    state: Arc<Mutex<PlanSnapshot>>,
}

impl InMemoryPlanState {
    /// Creates an empty revision-zero plan state.
    #[must_use]
    pub fn new() -> Self {
        static NEXT_PLAN_STATE: AtomicU64 = AtomicU64::new(1);
        let sequence = NEXT_PLAN_STATE.fetch_add(1, Ordering::Relaxed);
        // The authority is `memory.plan.<pid>.<counter>`: digits and dots only,
        // well under the length bound, so it is a valid identity by
        // construction. A revision of 1 is nonzero.
        let identity =
            PlanStateIdentity::new(format!("memory.plan.{}.{sequence}", std::process::id()), 1)
                .unwrap_or_else(|_| unreachable!("process-local plan authority is always valid"));
        Self::with_identity(identity)
    }

    /// Creates empty state with an application-selected authority identity.
    #[must_use]
    pub fn with_identity(identity: PlanStateIdentity) -> Self {
        Self {
            identity,
            state: Arc::new(Mutex::new(PlanSnapshot::empty())),
        }
    }
}

impl Default for InMemoryPlanState {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for InMemoryPlanState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryPlanState")
            .field("state", &"<bound>")
            .finish()
    }
}

impl PlanState for InMemoryPlanState {
    fn identity(&self) -> PlanStateIdentity {
        self.identity.clone()
    }

    fn snapshot(&self) -> Result<PlanSnapshot, ToolRuntimeError> {
        Ok(lock_plan(&self.state).clone())
    }

    fn replace(&self, replacement: PlanReplacement) -> PlanStateFuture {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let mut current = lock_plan(&state);
            if replacement
                .expected_revision
                .is_some_and(|revision| revision != current.revision)
            {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "plan revision conflict",
                ));
            }
            let revision = current.revision.checked_add(1).ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::ResourceLimit,
                    "plan revision is exhausted",
                )
            })?;
            *current = PlanSnapshot {
                revision,
                explanation: replacement.explanation,
                items: replacement.items,
            };
            Ok(current.clone())
        })
    }
}

fn lock_plan(state: &Mutex<PlanSnapshot>) -> std::sync::MutexGuard<'_, PlanSnapshot> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Clone)]
pub(crate) struct UpdatePlanTool {
    state: Arc<dyn PlanState>,
}

impl UpdatePlanTool {
    pub(crate) fn new(
        state: Arc<dyn PlanState>,
    ) -> Result<(ToolDefinition, Self), ToolRuntimeError> {
        Ok((update_plan_definition()?, Self { state }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdatePlanArgs {
    plan: Vec<PlanItem>,
}

impl ToolExecutor for UpdatePlanTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let args: UpdatePlanArgs = decode_arguments(&invocation)?;
            let replacement = match PlanReplacement::new(None, None, args.plan) {
                Ok(replacement) => replacement,
                Err(error) => return model_visible_plan_content_error(error),
            };
            context
                .progress()
                .report("updating plan state", None, None)?;
            let snapshot = self.state.replace(replacement).await?;
            context.check_cancelled()?;
            ToolOutcome::success_text(format_plan_snapshot(&snapshot))
        })
    }
}

#[derive(Clone)]
struct MergePlanTool {
    state: Arc<dyn PlanState>,
    id_steps: Arc<Mutex<BTreeMap<String, String>>>,
}

impl MergePlanTool {
    pub(crate) fn new(
        state: Arc<dyn PlanState>,
    ) -> Result<(ToolDefinition, Self), ToolRuntimeError> {
        let name = ToolName::new(MERGE_PLAN_CAPABILITY_NAME.to_owned()).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "merge_plan name is invalid",
                &error,
            )
        })?;
        let schema = object_schema(
            json!({
                "merge": {"type": "boolean"},
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "content": {"type": "string"},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"]
                            }
                        },
                        "required": ["id"],
                        "additionalProperties": false
                    }
                }
            }),
            &["todos"],
        );
        let definition = ToolDefinition::new(
            name,
            STANDARD_CODING_TOOL_VERSION,
            PLAKOUS_MERGE_PLAN_DESCRIPTION,
            ToolInputContract::structured(schema).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::InvalidDefinition,
                    "merge_plan input contract is invalid",
                    &error,
                )
            })?,
            ToolOutputContract::model_content(128, 1_048_576).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::InvalidDefinition,
                    "merge_plan output contract is invalid",
                    &error,
                )
            })?,
            workspace_concurrency(&["harness.plan"])?,
            ToolEffect::Mutating,
        )
        .map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "merge_plan definition is invalid",
                &error,
            )
        })?;
        Ok((
            definition,
            Self {
                state,
                id_steps: Arc::new(Mutex::new(BTreeMap::new())),
            },
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergePlanArgs {
    #[serde(default = "default_grok_merge")]
    merge: bool,
    todos: Vec<Value>,
}

const fn default_grok_merge() -> bool {
    true
}

impl ToolExecutor for MergePlanTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let args: MergePlanArgs = decode_arguments(&invocation)?;
            let snapshot = self.state.snapshot()?;
            let plan = match merge_todos_into_plan(
                args.merge,
                &args.todos,
                snapshot.items(),
                &mut lock_id_steps(&self.id_steps),
            ) {
                Ok(plan) => plan,
                Err(error) => return model_visible_plan_content_error(error),
            };
            let replacement = match PlanReplacement::new(None, None, plan) {
                Ok(replacement) => replacement,
                Err(error) => return model_visible_plan_content_error(error),
            };
            context
                .progress()
                .report("updating plan state", None, None)?;
            let snapshot = self.state.replace(replacement).await?;
            context.check_cancelled()?;
            ToolOutcome::success_text(format_plan_snapshot(&snapshot))
        })
    }
}

pub(crate) fn merge_plan_registration(
    plan_state: Arc<dyn PlanState>,
) -> Result<ToolRegistration, ToolRuntimeError> {
    let (definition, executor) = MergePlanTool::new(plan_state)?;
    let executor_id = ToolExecutorId::new("plakous.merge-plan.v1")?;
    Ok(ToolRegistration::new(
        RegisteredToolId::new("plakous.merge-plan.v1")?,
        executor_id.clone(),
        ToolExecutorBindingIdentity::new(4, &[b"merge-plan", executor_id.as_str().as_bytes()])?,
        definition,
        Arc::new(executor),
    )
    .nested_eligible())
}

fn lock_id_steps(
    state: &Mutex<BTreeMap<String, String>>,
) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn format_plan_snapshot(snapshot: &PlanSnapshot) -> String {
    let mut text = format!("revision={}\n", snapshot.revision());
    if let Some(explanation) = snapshot.explanation() {
        text.push_str("explanation: ");
        text.push_str(explanation);
        text.push('\n');
    }
    for item in snapshot.items() {
        let status = match item.status() {
            PlanItemStatus::Pending => "pending",
            PlanItemStatus::InProgress => "in_progress",
            PlanItemStatus::Completed => "completed",
        };
        text.push_str("- [");
        text.push_str(status);
        text.push_str("] ");
        text.push_str(item.step());
        text.push('\n');
    }
    text
}

fn merge_todos_into_plan(
    merge: bool,
    todos: &[Value],
    current: &[PlanItem],
    id_steps: &mut BTreeMap<String, String>,
) -> Result<Vec<PlanItem>, ToolRuntimeError> {
    let mut by_id = BTreeMap::new();
    if merge {
        if id_steps.is_empty() && !current.is_empty() {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                PLAKOUS_TOOL_MERGE_PLAN_COLD_MAP,
            ));
        }
        if !id_steps.is_empty() {
            for item in current {
                for (id, step) in id_steps.iter() {
                    if step == item.step() {
                        by_id.insert(id.clone(), item.clone());
                    }
                }
            }
        }
    } else {
        id_steps.clear();
    }
    for todo in todos {
        let object = todo.as_object().ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "todo_write item is not an object",
            )
        })?;
        let id = object.get("id").and_then(Value::as_str).ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "todo_write item is missing id",
            )
        })?;
        let status_raw = object.get("status").and_then(Value::as_str);
        if status_raw == Some("cancelled") {
            by_id.remove(id);
            id_steps.remove(id);
            continue;
        }
        let status = status_raw.ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "todo_write item is missing status",
            )
        })?;
        let content = object
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let step = if let Some(content) = content {
            content
        } else if let Some(existing) = by_id.get(id) {
            existing.step().to_owned()
        } else if let Some(step) = id_steps.get(id).cloned() {
            step
        } else {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "todo_write item is missing content for a new id",
            ));
        };
        let item = PlanItem::new(step, parse_merge_plan_status(status)?)?;
        id_steps.insert(id.to_owned(), item.step().to_owned());
        by_id.insert(id.to_owned(), item);
    }
    let plan = by_id.into_values().collect::<Vec<_>>();
    if plan.is_empty() {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "todo_write produced an empty plan",
        ));
    }
    Ok(plan)
}

/// Invalid plan content is a model-visible tool failure, not a lost mutating
/// dispatch. `todo_write` / `merge_plan` are `ToolEffect::Mutating`; an `Err`
/// after executor start is widened to `OutcomeUnknown` and recovery-required,
/// which is how four `in_progress` items killed a live Grok turn.
fn model_visible_plan_content_error(
    error: ToolRuntimeError,
) -> Result<ToolOutcome, ToolRuntimeError> {
    match error.kind() {
        ToolRuntimeErrorKind::InvalidArguments | ToolRuntimeErrorKind::ResourceLimit => {
            ToolOutcome::failure_text(error.message().to_string())
        }
        _ => Err(error),
    }
}

fn parse_merge_plan_status(status: &str) -> Result<PlanItemStatus, ToolRuntimeError> {
    match status {
        "pending" => Ok(PlanItemStatus::Pending),
        "in_progress" => Ok(PlanItemStatus::InProgress),
        "completed" => Ok(PlanItemStatus::Completed),
        _ => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "todo_write status is invalid",
        )),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::error::Error;

    use super::*;

    #[test]
    fn canonical_update_plan_schema_matches_main() {
        let expected = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "plan": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": {"type": "string"},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["step", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["plan"],
            "additionalProperties": false
        });
        assert_eq!(PlanToolPack::update_plan_input_schema(), expected);
    }

    #[test]
    fn invalid_plan_content_is_a_model_visible_failure_not_a_lost_dispatch() {
        let error = ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "plan may contain at most one in-progress item",
        );
        let outcome = model_visible_plan_content_error(error).expect("failure text");
        assert_eq!(outcome.kind(), plakous_tools::ToolOutcomeKind::Failure);
    }

    #[test]
    fn grok_oauth_todo_merge_flips_status_by_id_only() -> Result<(), Box<dyn Error>> {
        let mut id_steps = BTreeMap::from([("a".to_owned(), "step".to_owned())]);
        let current = vec![PlanItem::new("step", PlanItemStatus::Pending)?];
        let todos = vec![json!({"id": "a", "status": "completed"})];
        let plan = merge_todos_into_plan(true, &todos, &current, &mut id_steps)?;
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].step(), "step");
        assert_eq!(plan[0].status(), PlanItemStatus::Completed);
        Ok(())
    }

    #[test]
    fn grok_oauth_todo_merge_fails_closed_on_cold_map_with_existing_snapshot() {
        let mut id_steps = BTreeMap::new();
        let current = vec![PlanItem::new("step", PlanItemStatus::Pending).expect("item")];
        let todos = vec![json!({"id": "a", "status": "completed"})];
        let error = merge_todos_into_plan(true, &todos, &current, &mut id_steps)
            .expect_err("cold merge must fail closed");
        assert_eq!(error.kind(), ToolRuntimeErrorKind::InvalidArguments);
        assert_eq!(error.message().as_str(), PLAKOUS_TOOL_MERGE_PLAN_COLD_MAP);
    }

    #[test]
    fn grok_oauth_todo_replace_on_cold_map_with_existing_snapshot() -> Result<(), Box<dyn Error>> {
        let mut id_steps = BTreeMap::new();
        let current = vec![PlanItem::new("old step", PlanItemStatus::Pending)?];
        let todos = vec![json!({
            "id": "new",
            "content": "new step",
            "status": "pending"
        })];
        let plan = merge_todos_into_plan(false, &todos, &current, &mut id_steps)?;
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].step(), "new step");
        assert_eq!(id_steps.get("new").map(String::as_str), Some("new step"));
        Ok(())
    }

    #[test]
    fn grok_oauth_todo_merge_false_replaces_existing_plan() -> Result<(), Box<dyn Error>> {
        let mut id_steps = BTreeMap::from([("old".to_owned(), "old step".to_owned())]);
        let current = vec![PlanItem::new("old step", PlanItemStatus::Pending)?];
        let todos = vec![json!({
            "id": "new",
            "content": "new step",
            "status": "in_progress"
        })];
        let plan = merge_todos_into_plan(false, &todos, &current, &mut id_steps)?;
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].step(), "new step");
        assert_eq!(plan[0].status(), PlanItemStatus::InProgress);
        assert!(!id_steps.contains_key("old"));
        assert_eq!(id_steps.get("new").map(String::as_str), Some("new step"));
        Ok(())
    }

    #[test]
    fn grok_oauth_todo_cancelled_removes_item() -> Result<(), Box<dyn Error>> {
        let mut id_steps = BTreeMap::from([("a".to_owned(), "step".to_owned())]);
        let current = vec![PlanItem::new("step", PlanItemStatus::Pending)?];
        let todos = vec![json!({"id": "a", "status": "cancelled"})];
        let err = merge_todos_into_plan(true, &todos, &current, &mut id_steps)
            .expect_err("cancelled-only update must not leave an empty plan");
        assert_eq!(err.kind(), ToolRuntimeErrorKind::InvalidArguments);
        assert!(!id_steps.contains_key("a"));
        Ok(())
    }

    #[test]
    fn public_plan_snapshot_construction_revalidates_private_items() {
        let invalid_steps = [String::new(), "é".repeat(MAX_STANDARD_PLAN_STEP_BYTES)];
        for step in invalid_steps {
            let result = PlanSnapshot::new(
                1,
                None,
                vec![PlanItem {
                    step,
                    status: PlanItemStatus::Pending,
                }],
            );
            assert_eq!(
                result.err().map(|error| error.kind()),
                Some(ToolRuntimeErrorKind::ResourceLimit)
            );
        }
    }

    #[tokio::test]
    async fn plan_state_is_revisioned_and_conflict_safe() -> Result<(), ToolRuntimeError> {
        let plan_state = InMemoryPlanState::new();
        let first = plan_state
            .replace(PlanReplacement::new(
                Some(0),
                Some(String::from("start")),
                vec![PlanItem::new("work", PlanItemStatus::InProgress)?],
            )?)
            .await?;
        assert_eq!(first.revision(), 1);
        let conflict = plan_state
            .replace(PlanReplacement::new(
                Some(0),
                None,
                vec![PlanItem::new("done", PlanItemStatus::Completed)?],
            )?)
            .await;
        assert_eq!(
            conflict.err().map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::Execution)
        );
        assert_eq!(plan_state.snapshot()?.revision(), 1);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The plan pack.
// ---------------------------------------------------------------------------

/// A stateful port with its own identity and revision protocol: the bounded
/// task plan, its replacement tool, and the id-merge `todo_write` lowering.
#[derive(Clone, Default)]
pub struct PlanToolPack {
    plan_state: Option<Arc<dyn PlanState>>,
}

impl fmt::Debug for PlanToolPack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlanToolPack")
            .field("registers", &self.plan_state.is_some())
            .finish()
    }
}

impl PlanToolPack {
    /// The pack over one application-owned plan-state port.
    #[must_use]
    pub fn new(plan_state: Arc<dyn PlanState>) -> Self {
        Self {
            plan_state: Some(plan_state),
        }
    }

    /// The pack as a menu over an already registered plan capability.
    #[must_use]
    pub fn menu_only() -> Self {
        Self { plan_state: None }
    }

    /// The frozen canonical `update_plan` input schema.
    #[must_use]
    pub fn update_plan_input_schema() -> Value {
        update_plan_input_schema()
    }
}

impl ToolPack for PlanToolPack {
    fn id(&self) -> &'static str {
        "plakous.plan.v3"
    }

    /// `update_plan` binds the plan-state port's own executor identity, so the
    /// durable descriptor names the authority the plan lives under; `merge_plan`
    /// is the id-merge lowering the Grok `todo_write` presentation needs.
    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        let Some(plan_state) = self.plan_state.as_ref() else {
            return Ok(Vec::new());
        };
        let executor_id = plan_state.identity().executor_id()?;
        let (definition, executor) = UpdatePlanTool::new(Arc::clone(plan_state))?;
        Ok(vec![
            standard_registration_with_executor(
                "update-plan",
                definition,
                Arc::new(executor),
                &executor_id,
            )?,
            merge_plan_registration(Arc::clone(plan_state))?,
        ])
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one reviewed menu table per presentation"
    )]
    fn menu(&self, presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        let update_plan = tool_name(
            UPDATE_PLAN_CAPABILITY_NAME,
            "canonical coding capability name is invalid",
        )?;
        Ok(match presentation {
            CodingPresentation::Codex => vec![MenuEntry::new(
                "update_plan",
                vec![MenuBinding::by_effect(update_plan)],
                MenuShape::Structured {
                    description: Arc::from(PLAKOUS_CODEX_UPDATE_PLAN_DESCRIPTION),
                    schema: object_schema(
                        json!({
                            "plan": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "step": {"type": "string"},
                                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                                    },
                                    "required": ["step", "status"],
                                    "additionalProperties": false
                                }
                            }
                        }),
                        &["plan"],
                    ),
                },
                ToolProjectionLoweringV2::CanonicalPassthrough,
            )],
            CodingPresentation::Grok => {
                let merge_plan = tool_name(
                    MERGE_PLAN_CAPABILITY_NAME,
                    "merge_plan capability name is invalid",
                )?;
                vec![MenuEntry::new(
                    "todo_write",
                    vec![MenuBinding::exact(
                        CanonicalProcessOperationV2::Start,
                        merge_plan,
                    )],
                    MenuShape::Structured {
                        description: Arc::from(PLAKOUS_GROK_TODO_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "merge": {"type": "boolean"},
                                "todos": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "id": {"type": "string"},
                                            "content": {"type": "string"},
                                            "status": {
                                                "type": "string",
                                                "enum": ["pending", "in_progress", "completed", "cancelled"]
                                            }
                                        },
                                        "required": ["id"],
                                        "additionalProperties": false
                                    }
                                }
                            }),
                            &["todos"],
                        ),
                    },
                    ToolProjectionLoweringV2::CanonicalPassthrough,
                )]
            }
            // Neither Claude Code's five tools nor pi carry a plan tool.
            CodingPresentation::Claude | CodingPresentation::Pi => Vec::new(),
        })
    }

    fn pins(&self) -> &'static [PromptPin] {
        PLAN_PROMPT_PINS
    }
}

/// Every model-visible string the plan pack owns.
static PLAN_PROMPT_PINS: &[PromptPin] = &[
    PromptPin::new(
        "plan.update_plan",
        PLAKOUS_TOOL_UPDATE_PLAN_DESCRIPTION,
        "02d06bf620bfaca6013e7bfcd4fef199092a8c7652d528f29bb9842b228cf200",
    ),
    PromptPin::new(
        "plan.merge_plan",
        PLAKOUS_MERGE_PLAN_DESCRIPTION,
        "fb0833f8959992f415a0968d5aa01fb15744154b3eaafdc66af1de2c691cfa48",
    ),
    PromptPin::new(
        "plan.codex.update_plan",
        PLAKOUS_CODEX_UPDATE_PLAN_DESCRIPTION,
        "e71fc56d035e8595e1b006fd02318a7318998243f3ab441160fa08b9cff81a02",
    ),
    PromptPin::new(
        "plan.grok.todo_write",
        PLAKOUS_GROK_TODO_DESCRIPTION,
        "626bd025d23f14606bfc62d562f95f711b767297d8ca85ae2393ecfdf6202ff3",
    ),
];
