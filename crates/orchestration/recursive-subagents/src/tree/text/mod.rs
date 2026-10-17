//! The tree's model-visible text: tool descriptions, the delegation policy
//! sentences, the template-card header lines, and the refusal messages.
//!
//! Tool descriptions are the model-visible protocol — the digest test below
//! asserts the text quotes the protocol's own constants — so the canonical
//! strings belong to the tree rather than to a prompt costume. A prompts crate
//! that wants to re-voice a description constructs its own table and owns its
//! own pins; nothing here is edited from outside.

pub mod errors;

#[cfg(any(test, feature = "test-support"))]
use plakous::tools::PromptPin;

use crate::protocol::Delegation;

/// Every model-visible string this module owns.
#[cfg(any(test, feature = "test-support"))]
pub static TOPOLOGY_PROMPT_PINS: &[PromptPin] = &[
    PromptPin::new(
        "topology.topology_spawn_description_template",
        PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE,
        "085549c413b96c9fd1a03ed8c83e2508427f6d5488bf8566cd20ed9e850b39ff",
    ),
    PromptPin::new(
        "topology.topology_send_description",
        PLAK_TOPOLOGY_SEND_DESCRIPTION,
        "92a0b4f7c1733b92d8ffce7ae4a1c8d39be16f013819848e0069a51690dc405f",
    ),
    PromptPin::new(
        "topology.topology_list_description",
        PLAK_TOPOLOGY_LIST_DESCRIPTION,
        "373965595eeeba4063668171a705ef5da37e3382b8d2432cb60213a59a56b5a5",
    ),
    PromptPin::new(
        "topology.topology_wait_description",
        PLAK_TOPOLOGY_WAIT_DESCRIPTION,
        "1dc3568f4a3d1e04091ce3d6dfb7858199ad51bdd0d75360f1a23a88727660b1",
    ),
    PromptPin::new(
        "topology.topology_submit_description",
        PLAK_TOPOLOGY_SUBMIT_DESCRIPTION,
        "f6cff03756b249e356bbab7ffb816a39999f07307f3e83510ef81a3a64eaa384",
    ),
    PromptPin::new(
        "topology.topology_interrupt_description",
        PLAK_TOPOLOGY_INTERRUPT_DESCRIPTION,
        "2c84ab6bfe4d8f45b7bafa5d02a28bf2e8dee9cb75a3a16719abf1d8305921a6",
    ),
    PromptPin::new(
        "topology.topology_task_output_description",
        PLAK_TOPOLOGY_TASK_OUTPUT_DESCRIPTION,
        "42fa7244e63f59e74c4648109fad21e6a3970b60ec52056796a23794905ccf39",
    ),
    PromptPin::new(
        "topology.topology_task_kill_description",
        PLAK_TOPOLOGY_TASK_KILL_DESCRIPTION,
        "adc002d996612ff0b135387465f25f679e6862c384788210df03c18022c29033",
    ),
    PromptPin::new(
        "topology.topology_jobs_wait_description",
        PLAK_TOPOLOGY_JOBS_WAIT_DESCRIPTION,
        "eb9a9ab757fa4152cee77b4740da08442ef1224a9f2f70a8c560a412e2b57a83",
    ),
    PromptPin::new(
        "topology.topology_jobs_stop_description",
        PLAK_TOPOLOGY_JOBS_STOP_DESCRIPTION,
        "416b3f1f1ed6dd0f23380a56010665b708eb47b5e83c7baf644f5ceb414167d0",
    ),
    PromptPin::new(
        "topology.topology_close_description",
        PLAK_TOPOLOGY_CLOSE_DESCRIPTION,
        "0cc2e5859d0f73fb3b255bb61d2dd2d6d3c601e992302bf912a0ff0d158b5fc4",
    ),
    PromptPin::new(
        "topology.topology_recover_description",
        PLAK_TOPOLOGY_RECOVER_DESCRIPTION,
        "dc2e20bd99fec5e5b63011d3d8014610afdf2969de7d42e34f2a0b75f2bccf74",
    ),
    PromptPin::new(
        "topology.topology_adopt_description_template",
        PLAK_TOPOLOGY_ADOPT_DESCRIPTION_TEMPLATE,
        "cd863ea693132abc226812465b3de9d34bddc3ff3bc76e441653cc6bfe6c3942",
    ),
    PromptPin::new(
        "topology.topology_discard_description",
        PLAK_TOPOLOGY_DISCARD_DESCRIPTION,
        "5ee4c40111d8f7039b272a8690e1a98a1006f537cf3c77e7028d1edaefcdc283",
    ),
    PromptPin::new(
        "topology.topology_operation_fallback_template",
        PLAK_TOPOLOGY_OPERATION_FALLBACK_TEMPLATE,
        "1237cbd22336b831171e657069dc3d0d5d74cd15772334060cea385deeafae77",
    ),
    PromptPin::new(
        "topology.delegation_explicit",
        PLAK_DELEGATION_EXPLICIT,
        "cc2c73c60f62806e45cacf16090f4079645297c82075afe09ebbfe42fb02b922",
    ),
    PromptPin::new(
        "topology.delegation_proactive",
        PLAK_DELEGATION_PROACTIVE,
        "f457fd472e191b970c922f828076bfa6d9a8638643043971a9701e911a70688a",
    ),
    PromptPin::new(
        "topology.template_cards_header",
        PLAK_TEMPLATE_CARDS_HEADER,
        "397400e9c2ef573c9c0ead48d9bb19238a484278647daf729b07baac97e7363d",
    ),
    PromptPin::new(
        "topology.template_cards_none",
        PLAK_TEMPLATE_CARDS_NONE,
        "159c4047c113ad6a0c817196dd57ad6cc24f05ebb75421732da5d83a01272554",
    ),
];

/// `spawn_agent` tool description template; `{await}` is the presented await
/// tool, `{nodes}` and `{children}` are the caller's granted tree and
/// direct-child bounds, and `{cards}` is the working-set template block.
pub const PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE: &str = "Spawn a child from a template in the working set. Required fields: template, prompt, output_schema_json, tier. The receipt is a running child with an attempt_id, not a result. Child results and questions arrive as mail; {await} returns them. Default: children share your checkout. Give them file-disjoint work, or run writers in sequence. Pass workspace=fork when more than one child must write at the same time, or when the work should stay off this checkout. tier is clipped to the template ceiling: read_only refuses Plak file writes; mutate allows them. Settlement is not success: output_json is present only after submit_result. Do not call send_agent_message to start it. output_schema_json is a JSON string for the child's submit_result object, like {\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"]}. Caps: {nodes} live nodes, depth 4, {children} undelivered or active direct children; messages ≤16384 bytes; output_json ≤262144 bytes.\n\n{cards}";

/// `send_agent_message` tool description on hub roots with topology granted.
pub const PLAK_TOPOLOGY_SEND_DESCRIPTION: &str = "Send a message to another agent. A child is already working from spawn: mail is a follow-up on its running turn (priority=urgent steers; omitted priority is urgent from a parent to its running child, deferred otherwise), not how the prompt is assigned. A finished child takes purpose=followup, which re-tasks it on a new attempt with a new turn token and, if you give one, a new output_schema_json. purpose=reply requires in_reply_to set to a message_id from list_agents inbox[]; in_reply_to only when purpose=reply.";

/// `list_agents` tool description on hub roots with topology granted.
pub const PLAK_TOPOLOGY_LIST_DESCRIPTION: &str = "List visible agents with description and prompt_bytes, not prompt text. inbox[] includes only unread messages addressed to you; include_read=true adds the newest 64 acknowledged messages per recipient, newest first. inbox[] body previews are capped at 2048 bytes; full model-authored bodies (up to 16384 bytes) reach session delivery and a parent's await. Result notices carry output_bytes and output_truncated; parents await the child for the full output. Send purpose=reply with in_reply_to set to inbox[].message_id.";

/// `wait_agent` tool description on hub roots with topology granted.
pub const PLAK_TOPOLOGY_WAIT_DESCRIPTION: &str = "Wait until any watched child settles or sends you mail (interrupted also settles). Omit agent_ids to watch the children that have not reported yet: each result comes back once, and no_pending_children means all have. Returns a row per watched id with its attempt status, mail[] in arrival order with full bodies and message ids of unread mail it consumed (result mail has only message_id, purpose, attempt_id when its result is in agents[]), and woke_by (settled, mail, timeout). Returned mail is acknowledged once; later waits do not wake on it again. Child results and questions arrive as mail; wait_agent returns them. A human or unwatched-parent steer returns woke_by=steer without its text; watched urgent mail still returns woke_by=mail with its body. If one of your own background processes becomes terminal after this wait starts, it returns woke_by=process with processes naming those ids; already-terminal processes do not wake it again. Process completion is reported automatically. Naming delivered and running children together waits for new mail or settlement; naming only delivered children re-reads without parking. Settlement is not success: relay output_json from rows that submitted; completed_without_submission has none. Settlement does not change your checkout. timed_out: true means none settled; call again. timeout_ms defaults to 300000, max 3600000.";

/// `submit_result` tool description on child agents with a delegated attempt.
pub const PLAK_TOPOLOGY_SUBMIT_DESCRIPTION: &str = "Submit this attempt's result. Required for every child, including supervisors; omitting it is completed_without_submission. Assistant text is not the result.";

/// `interrupt_agent` tool description on hub roots with topology granted.
pub const PLAK_TOPOLOGY_INTERRUPT_DESCRIPTION: &str = "Cancel the named child's current turn only; any open wait_agent returns with status interrupted; the node stays listable and can receive follow-up mail; there is no separate resume tool.";

/// Grok's `get_command_or_subagent_output` on a node granted agent tools:
/// the await over children and the node's own processes.
pub const PLAK_TOPOLOGY_TASK_OUTPUT_DESCRIPTION: &str = "Get the output and status of background commands and child agents. Pass task_ids: process ids returned by run_terminal_command (N or pN) and child agent ids. Completion is reported automatically; pass a positive timeout_ms only when this turn needs a result, clamped to 3600000 ms (1 hour). Omit timeout_ms or pass 0 for an immediate snapshot. A steer returns early without interrupting anything. Only process ids: tasks[] with each task id, output, status and exit code when done; only terminal observations consume a completion notice. Naming a child returns result, a JSON string (called from code, the object itself) with agents[] (a row per named child with its attempt status, and output_json when it submitted), mail[] (unread mail it consumed, in arrival order, acknowledged once), jobs[] (a snapshot row per named process; it consumes no completion notice), timed_out and woke_by (settled, mail, timeout, steer, process). It returns when a named child settles or sends mail, or a named process ends. Settlement is not success: relay output_json only from rows that submitted.";

/// Grok's `kill_command_or_subagent` on a node granted agent tools: a stop
/// for the node's own processes, an interrupt for its children.
pub const PLAK_TOPOLOGY_TASK_KILL_DESCRIPTION: &str = "Stop a background command or interrupt a child agent by task_id. A process id (N or pN) gets SIGTERM to its process group, then SIGKILL after a 2 second grace; it succeeds if the process stopped or had already exited, and a natural exit keeps its status. A child agent id cancels that child's current turn only: it stays listable, can take follow-up mail, and is not closed; close_agent ends it.";

/// Pi's `wait` on a node granted agent tools: the await over children and the
/// node's own processes.
pub const PLAK_TOPOLOGY_JOBS_WAIT_DESCRIPTION: &str = "Wait for background processes started by bash and for child agents. Pass ids: process ids (N or pN) and child agent ids. Omit ids to watch every child that has not reported and every own running process; with no child pending, or an own process that already finished unreported, a bare wait is the process wait over every unreported process (one that already finished returns at once). Completion is reported automatically: use wait only when this turn needs a result. timeout_ms defaults to 300000 and is clamped to 3600000 (1 hour); 0 returns a snapshot. A steer returns early without interrupting anything. A process-only wait returns tasks[] with each process's status, exit code and output so far when the first of them finishes; only finished processes consume their completion notice. A wait over children returns result, a JSON string (called from code, the object itself) with agents[] (a row per watched child with its attempt status, and output_json when it submitted), mail[] (unread mail it consumed, in arrival order, acknowledged once), jobs[] (a snapshot row per watched process; it consumes no completion notice), timed_out and woke_by (settled, mail, timeout, steer, process); it returns when a watched child settles or sends mail, or a watched process ends. Settlement is not success: relay output_json only from rows that submitted.";

/// Pi's `stop` on a node granted agent tools: a stop for the node's own
/// processes, an interrupt for its children.
pub const PLAK_TOPOLOGY_JOBS_STOP_DESCRIPTION: &str = "Stop a background process or interrupt a child agent by id. A process id returned by bash (N or pN) gets SIGTERM to its process group, then SIGKILL after a 2 second grace; it succeeds if the process stopped or had already exited, and a natural exit keeps its status. A child agent id cancels that child's current turn only: it stays listable, can take follow-up mail, and is not closed; close_agent ends it.";

/// `close_agent` tool description on hub roots with topology granted.
pub const PLAK_TOPOLOGY_CLOSE_DESCRIPTION: &str = "End the named child and its descendants permanently; closed rows stay inspectable in list_agents.";

/// `recover_agent` tool description on hub roots with topology granted.
pub const PLAK_TOPOLOGY_RECOVER_DESCRIPTION: &str = "Recover a child whose list_agents row says remedy recover_agent: its session is reopened exactly, nothing it ran is replayed, and its attempt ends failed so purpose=followup and close_agent work again. A recovery that cannot run yet is refused with a recovery_* code and changes nothing; repeating it on a recovered child changes nothing.";

/// `adopt_candidate` tool description on nodes granted `may_adopt`; `{await}`
/// is the presented await tool.
pub const PLAK_TOPOLOGY_ADOPT_DESCRIPTION_TEMPLATE: &str = "Integrate a sealed candidate from your subtree into your own workspace by candidate_id, as working-tree changes with no commit. A forked child's edits are not in your checkout until this succeeds. merge_conflict is a failed result and keeps the child copy; target_changed means your copy moved since expected_target. A {await} or list_agents row names the candidate once the child has sealed.";

/// `discard_workspace` tool description on spawn-capable owners.
pub const PLAK_TOPOLOGY_DISCARD_DESCRIPTION: &str = "Drop a child fork by workspace_id, giving up its unadopted candidates and their pins without adopting anything from it.";

/// Fallback topology tool description template for tools without bespoke copy; `{name}` is the tool name.
pub const PLAK_TOPOLOGY_OPERATION_FALLBACK_TEMPLATE: &str = "Product topology operation {name}.";

/// Delegation policy, `explicit`. Pinned model-visible policy text.
pub const PLAK_DELEGATION_EXPLICIT: &str = "Spawn sub-agents only when the user asks for delegation, sub-agents, or parallel work. Requests for depth, thoroughness, or research are not that permission. When asked, actually spawn; saying you will is not delegation. Otherwise do the work yourself.";

/// Delegation policy, `proactive`. Pinned model-visible policy text.
pub const PLAK_DELEGATION_PROACTIVE: &str = "Spawn sub-agents for independent subtasks when that keeps your own context clean or lets work run in parallel with yours. Do small or sequential work yourself. Choose the template from the cards below.";

/// Exactly one delegation sentence per spawn-capable node.
#[must_use]
pub const fn delegation_policy_sentence(delegation: Delegation) -> &'static str {
    match delegation {
        Delegation::Explicit => PLAK_DELEGATION_EXPLICIT,
        Delegation::Proactive => PLAK_DELEGATION_PROACTIVE,
    }
}

/// First line of template cards compiled into `spawn_agent` descriptions.
pub const PLAK_TEMPLATE_CARDS_HEADER: &str = "Spawn by template name only. Do not name accounts, models, or effort. Templates you may spawn:";

/// Card bullet when the working set leaves this node nothing to spawn.
pub const PLAK_TEMPLATE_CARDS_NONE: &str =
    "- (none; spawn_agent fails closed until [roster] working_set names another template)";

/// The capability-line segment naming the optional tools a template selects,
/// in the template's order, so a parent can route by capability. `None` when
/// the template selects none, which leaves the card unchanged.
pub fn template_card_tools<I>(names: I) -> Option<String>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let names = names
        .into_iter()
        .map(|name| name.as_ref().to_owned())
        .collect::<Vec<_>>();
    (!names.is_empty()).then(|| format!("tools {}", names.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        DEFAULT_WAIT_TIMEOUT_MS, MAX_MESSAGE_BYTES, MAX_OUTPUT_JSON_BYTES, MAX_TREE_DEPTH,
        MAX_WAIT_TIMEOUT_MS,
    };

    #[test]
    fn topology_spawn_limits_match_protocol_constants() {
        assert!(PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE.contains("{nodes} live nodes"));
        assert!(PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE.contains(&MAX_TREE_DEPTH.to_string()));
        assert!(
            PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE
                .contains("{children} undelivered or active direct children")
        );
        assert!(PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE.contains(&MAX_MESSAGE_BYTES.to_string()));
        assert!(
            PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE.contains(&MAX_OUTPUT_JSON_BYTES.to_string())
        );
        assert!(
            PLAK_TOPOLOGY_WAIT_DESCRIPTION.contains(&DEFAULT_WAIT_TIMEOUT_MS.to_string())
                && PLAK_TOPOLOGY_WAIT_DESCRIPTION.contains(&MAX_WAIT_TIMEOUT_MS.to_string())
        );
        assert!(PLAK_TOPOLOGY_TASK_OUTPUT_DESCRIPTION.contains(&MAX_WAIT_TIMEOUT_MS.to_string()));
        assert!(
            PLAK_TOPOLOGY_JOBS_WAIT_DESCRIPTION.contains(&DEFAULT_WAIT_TIMEOUT_MS.to_string())
                && PLAK_TOPOLOGY_JOBS_WAIT_DESCRIPTION.contains(&MAX_WAIT_TIMEOUT_MS.to_string())
        );
    }

    /// Retired sentences must not come back through a constant.
    #[test]
    fn topology_descriptions_name_no_removed_surface() {
        for text in [
            PLAK_TOPOLOGY_SPAWN_DESCRIPTION_TEMPLATE,
            PLAK_TOPOLOGY_SEND_DESCRIPTION,
            PLAK_TOPOLOGY_WAIT_DESCRIPTION,
            PLAK_TOPOLOGY_ADOPT_DESCRIPTION_TEMPLATE,
            PLAK_TOPOLOGY_DISCARD_DESCRIPTION,
            PLAK_TEMPLATE_CARDS_HEADER,
            PLAK_TEMPLATE_CARDS_NONE,
            PLAK_DELEGATION_EXPLICIT,
            PLAK_DELEGATION_PROACTIVE,
        ] {
            for removed in [
                "merge_workspace",
                "workspace_mode",
                "write_effect",
                "new_worktree",
                "immediately",
                "until it settles",
                "roster class",
                "does not merge worktrees",
                "rejects it",
                "child_may_spawn",
            ] {
                assert!(!text.contains(removed), "{removed} in {text}");
            }
        }
    }

    /// each `delegation` value renders its own sentence, and
    /// exactly one of them.
    #[test]
    fn a_card_names_selected_tools_in_template_order_and_nothing_when_none() {
        assert_eq!(
            template_card_tools(["web_search", "x_search", "mcp:github"]).as_deref(),
            Some("tools web_search, x_search, mcp:github")
        );
        assert_eq!(
            template_card_tools(["mcp:linear/list_issues"]).as_deref(),
            Some("tools mcp:linear/list_issues")
        );
        assert_eq!(template_card_tools(Vec::<String>::new()), None);
    }

    #[test]
    fn each_delegation_value_renders_exactly_one_sentence() {
        for (delegation, expected) in [
            (Delegation::Explicit, PLAK_DELEGATION_EXPLICIT),
            (Delegation::Proactive, PLAK_DELEGATION_PROACTIVE),
        ] {
            let sentence = delegation_policy_sentence(delegation);
            assert_eq!(sentence, expected);
            assert_eq!(sentence.matches("Spawn sub-agents").count(), 1);
            let other = delegation_policy_sentence(match delegation {
                Delegation::Explicit => Delegation::Proactive,
                Delegation::Proactive => Delegation::Explicit,
            });
            assert_ne!(sentence, other);
            assert!(!sentence.contains(other));
        }
        assert!(PLAK_DELEGATION_EXPLICIT.starts_with("Spawn sub-agents only when the user asks"));
        assert!(PLAK_DELEGATION_PROACTIVE.starts_with("Spawn sub-agents for independent subtasks"));
    }

    #[test]
    fn topology_tool_descriptions_are_stable() {}
}
