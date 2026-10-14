//! The model-visible strings this pack owns.
//!
//! A tool's description and its refusals travel with the tool (spec 02 §6.3):
//! they change together and they are pinned together.

/// Canonical merge_plan capability description (Grok presentation overlays grok-build text).
pub const PLAKOUS_MERGE_PLAN_DESCRIPTION: &str = "Replace or merge a bounded task list by item id. When merge is true (default), update matching ids; when false, replace the list. Status cancelled removes an id.";

/// Codex OAuth update_plan tool description.
pub const PLAKOUS_CODEX_UPDATE_PLAN_DESCRIPTION: &str = "Replace the bounded task plan. Keep at most one in_progress step. Do not use this as a substitute for repository facts.";

/// Grok OAuth todo_write tool description.
pub const PLAKOUS_GROK_TODO_DESCRIPTION: &str = "Create and manage a structured task list. The user sees this list live — it is your primary way to show progress.\n\nUse for any task with 3+ steps. Skip for trivial single-step work. Keep at most one in_progress item.\n\nWhen merge is true (default), merges the provided todos into the existing list by id — send only the items you are changing, and to flip status without changing content send just id + status. When false, the provided todos replace the existing list. Status cancelled removes the item from the list.";

/// merge_plan tool, returned when merge=true after a cold id map with existing items.
pub const PLAKOUS_TOOL_MERGE_PLAN_COLD_MAP: &str = "merge cannot map existing plan items after a cold restart; send merge:false with the full replacement list";

/// First line of the note that restores the latest plan after a compaction.
pub const PLAKOUS_PLAN_RESTORED_NOTE_HEADER: &str = "Current plan, restored after compaction. It is the latest state of your task list: continue from it, update it with the plan tool as you go, and do not plan again from scratch.";
