//! Product identity lines appended to harness base instructions.

use std::path::Path;

use plakous_model::{ReasoningEffort, ToolName};
use plakous_tools::{
    CodingPresentation, Earned, MenuEntry, PromptContribution, PromptPin, ToolPack,
    ToolRuntimeError,
};

/// Genesis environment sentence template; `{root}`, `{cwd}`, `{os}`.
pub const PLAK_PRODUCT_ENVIRONMENT_TEMPLATE: &str = "Workspace root: {root}. cwd: {cwd}. OS: {os}.";

/// Model identity sentence template; `{model}` is the selected model id string.
pub const PLAK_PRODUCT_MODEL_IDENTITY_TEMPLATE: &str = "You are {model}. This turn's prompts/instructions identify you; earlier messages show session/agent history.";

/// Selected reasoning effort sentence template; `{effort}`.
///
/// Codex states the effort it is running at in its turn context. Plakous does
/// the same when the resolved profile names one, and says nothing when it does
/// not, rather than guessing a default the provider may not honour.
pub const PLAK_PRODUCT_EFFORT_TEMPLATE: &str = "Selected reasoning effort: {effort}.";

/// The execution truth, in Plakous's own words.
///
/// The vendored Codex sections (`vendor/codex-prompt`) were written for a
/// harness with a sandbox and an approval flow, and they are copied without
/// edit. This is the sentence that corrects them: it is part of the
/// environment block, so it is stated once, after the base, wherever the base
/// came from. It names no product: §6.2's law is that the product's own
/// identity is chrome for the operator, not a claim made to the model.
///
/// It says only what is true on every route, on every host, at every moment of
/// a session, because it is rendered unconditionally and a model that believes
/// a false containment claim errs in the unsafe direction:
///
/// - It does not promise containment. Process tools carry the operator's full
///   host authority on every supported platform (spec 04 §2.3); the
///   `ProcessContainment` descriptor, not a prompt sentence, is the authority.
/// - It says *by default* nothing pauses a call. A coordination effect raises
///   `EffectDecisionRequired` with an `approval` event the operator answers.
///   Spec 02 §3.2 carries the same qualifier for the same reason.
pub const PLAK_PRODUCT_EXECUTION_POLICY: &str = "This harness runs the tools you call directly against this workspace. By default nothing pauses a call to ask the user first, so read a command before you run it.";

/// Cross-vertical VCS policy; not git/jj/`gh`-specific and not a shell-tool recipe.
pub const PLAK_PRODUCT_VCS_POLICY: &str = "Commit or push only when the user asks.";

/// Always-on working agreements. Topology protocol is not here: the delegation contract
/// retires the topology paragraph on nodes that cannot spawn, so the spawner
/// and assignee paragraphs below are appended only when the node's compiled
/// tools earn them. Do not name tools that are not on the current surface.
pub const PLAK_PRODUCT_WORKING_AGREEMENTS: &str = "Work from tool results: do not claim an effect finished unless the returned result shows it. Do not narrate tool calls; human-facing text is the outcome. Stay inside the asked scope; do not add drive-by features or comments. Follow enrolled project instruction files and in-repo conventions when they exist. You may call independent presented tools together. Do not retry a deterministic invalid call with the same arguments; bounded retries of transient failures and supported polling are different (polling is not a repeated invalid call); after an ambiguous mutating failure, inspect the affected state before retrying. Local reversible work may proceed; hard-to-reverse or shared effects (force-push, history rewrite, broad deletes, dropping data) need an explicit user ask.";

/// Spawner-only paragraph, appended when `spawn_agent` is compiled for the
/// node (leaves get no topology paragraph at all).
pub const PLAK_PRODUCT_TOPOLOGY_AGREEMENTS: &str = "The child starts from the spawn prompt; send_agent_message is follow-up on a running turn, not how work is assigned. wait_agent returns when a watched child settles or sends mail, and returns and acknowledges only that mail, and settlement is not success: read output_json only from rows that submitted. Default: children share your checkout; give them file-disjoint work or run writers in sequence. Pass workspace=fork when more than one child must write at the same time, or when the work should stay off this checkout. A forked child's edits are not in your checkout until adopt_candidate succeeds. adopt_candidate and discard_workspace stay valid when presented. A plan or todo tool records your intentions only; it does not start, cancel, or finish child work.";

/// The spawner paragraph on a Grok node, whose await over children is
/// `get_command_or_subagent_output` (`wait_agent` is not presented there).
pub const PLAK_PRODUCT_TOPOLOGY_GROK_AGREEMENTS: &str = "The child starts from the spawn prompt; send_agent_message is follow-up on a running turn, not how work is assigned. get_command_or_subagent_output naming child ids with a positive timeout_ms returns when a named child settles or sends mail, and returns and acknowledges only that mail, and settlement is not success: read output_json only from rows that submitted. Default: children share your checkout; give them file-disjoint work or run writers in sequence. Pass workspace=fork when more than one child must write at the same time, or when the work should stay off this checkout. A forked child's edits are not in your checkout until adopt_candidate succeeds. adopt_candidate and discard_workspace stay valid when presented. A plan or todo tool records your intentions only; it does not start, cancel, or finish child work.";

/// The spawner paragraph on a pi node, whose await over children is pi's
/// `wait` (`wait_agent` is not presented there).
pub const PLAK_PRODUCT_TOPOLOGY_PI_AGREEMENTS: &str = "The child starts from the spawn prompt; send_agent_message is follow-up on a running turn, not how work is assigned. wait returns when a watched child settles or sends mail (with no ids it watches every child that has not reported), and returns and acknowledges only that mail, and settlement is not success: read output_json only from rows that submitted. stop on a child id interrupts its turn; it does not close the child. Default: children share your checkout; give them file-disjoint work or run writers in sequence. Pass workspace=fork when more than one child must write at the same time, or when the work should stay off this checkout. A forked child's edits are not in your checkout until adopt_candidate succeeds. adopt_candidate and discard_workspace stay valid when presented. A plan or todo tool records your intentions only; it does not start, cancel, or finish child work.";

/// Completion channel earned by a root whose host wakes it.
pub const PLAK_PRODUCT_CHILD_WAKE_AGREEMENTS: &str = "You may end your turn while children run: this attached root host wakes you when results or questions arrive while host continuation is enabled. Results and questions from children arrive as <child_message> blocks with your next turn's message. Use wait_agent when you need a result before you can continue in this turn.";

/// Completion channel earned by a hub child; the tree host wakes every child.
pub const PLAK_PRODUCT_HUB_WAKE_AGREEMENTS: &str = "You may end your turn while children run: their results, questions and mail start a new turn as <child_message> blocks. Use wait_agent only when you need a result to continue this turn. Do not submit_result until every child you started has reported.";

/// Root wake paragraph where a pack await replaced `wait_agent` (Grok or pi).
/// The only-when sentence is the process paragraph's, once, for children and
/// processes; this paragraph does not repeat it.
pub const PLAK_PRODUCT_CHILD_WAKE_PACK_AGREEMENTS: &str = "You may end your turn while children run: this attached root host wakes you when results or questions arrive while host continuation is enabled. Results and questions from children arrive as <child_message> blocks with your next turn's message.";

/// Hub wake paragraph where a pack await replaced `wait_agent` (Grok or pi).
pub const PLAK_PRODUCT_HUB_WAKE_PACK_AGREEMENTS: &str = "You may end your turn while children run: their results, questions and mail start a new turn as <child_message> blocks. Do not submit_result until every child you started has reported.";

/// Completion channel for a spawner whose host does not wake it.
pub const PLAK_PRODUCT_CHILD_WAIT_AGREEMENTS: &str = "Call wait_agent to learn of child results. Call wait_agent until every child you started has reported before you submit_result or end your turn.";

/// [`PLAK_PRODUCT_CHILD_WAIT_AGREEMENTS`] on a Grok node.
pub const PLAK_PRODUCT_CHILD_WAIT_GROK_AGREEMENTS: &str = "Call get_command_or_subagent_output naming child ids, with nonzero timeout_ms, to learn of child results. Call it until every child you started has reported before you submit_result or end your turn.";

/// [`PLAK_PRODUCT_CHILD_WAIT_AGREEMENTS`] on a pi node.
pub const PLAK_PRODUCT_CHILD_WAIT_PI_AGREEMENTS: &str = "Call wait to learn of child results. Call wait until every child you started has reported before you submit_result or end your turn.";

/// Earned by an assignee whose host wakes it: parking is not submission.
pub const PLAK_PRODUCT_ASSIGNMENT_WAKE_AGREEMENTS: &str =
    "Ending a turn to wait is not a result: call submit_result when the assignment is done.";

const PROCESS_CODEX_WAKE_AGREEMENTS: &str = "When only waiting remains, end this turn; completion notices start a new turn. Use write_stdin to wait only when you need the output to continue this turn. Never sleep-poll; stop servers you no longer need before ending the turn.";

const PROCESS_GROK_WAKE_AGREEMENTS: &str = "When only waiting remains, end this turn; completion notices start a new turn. Use get_command_or_subagent_output with nonzero timeout_ms only when you need a result to continue this turn. Never sleep-poll; stop servers you no longer need before ending the turn.";

/// Claude Code 2.1.293's background wording: notified on completion, do not poll.
const PROCESS_CLAUDE_WAKE_AGREEMENTS: &str = "When only waiting remains, end this turn: you'll be notified when a background Bash command finishes, so do not poll. Give Bash a timeout (up to 600000 ms) only when you need the output to continue this turn. Stop servers you no longer need before ending the turn.";

/// The pi pack's three lines: background jobs report automatically; end the
/// turn when only waiting remains; `wait` only when blocked inside the turn.
const PROCESS_PI_WAKE_AGREEMENTS: &str = "Background jobs report automatically: when only waiting remains, end this turn; completion notices start a new turn. Use wait only when you need a result to continue this turn. Never poll or sleep; stop servers you no longer need before ending the turn.";

const PROCESS_CODEX_WAIT_AGREEMENTS: &str = "Use write_stdin to wait. Never sleep-poll. Long commands may use longer hints; stop unneeded servers.";

const PROCESS_GROK_WAIT_AGREEMENTS: &str = "Use get_command_or_subagent_output to wait, with nonzero timeout_ms. Never sleep-poll. Long commands may use longer hints; stop unneeded servers.";

const PROCESS_PI_WAIT_AGREEMENTS: &str = "Background jobs report automatically. Use wait when you need a result to continue this turn. Never poll or sleep; stop unneeded servers.";

const PROCESS_CLAUDE_WAIT_AGREEMENTS: &str = "Use Bash to wait: give a long command a timeout long enough to finish instead of running it in the background. Never sleep-poll; stop unneeded servers.";

/// Assignee-only paragraph, appended when `submit_result` is compiled for the
/// node. The genesis Start prompt carries the full contract; this is the line
/// that survives into later turns.
pub const PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS: &str = "Your turn is an assignment: submit_result is the outcome, using the assignment schema. Assistant text and native plan or todo updates are not a result.";

/// The first line of the identity tail. Production assembles the tail once
/// ([`crate::plan::PromptPlan::assemble`]) and never searches for it; the
/// render pins in the crate above use the marker to assert the tail is present
/// exactly once.
pub const PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER: &str = "Workspace root:";

/// One genesis environment sentence from resolved workspace paths.
#[must_use]
pub fn product_environment_sentence(workspace_root: &Path, cwd: &Path) -> String {
    PLAK_PRODUCT_ENVIRONMENT_TEMPLATE
        .replace("{root}", &workspace_root.display().to_string())
        .replace("{cwd}", &cwd.display().to_string())
        .replace("{os}", std::env::consts::OS)
}

/// The word the environment block uses for a resolved reasoning effort.
///
/// One site, in the crate that owns model-visible wording, so the effort
/// sentence reads the same on every route. A token budget is spelled out
/// rather than reduced to a level it does not correspond to.
#[must_use]
pub fn product_effort_label(effort: ReasoningEffort) -> String {
    match effort {
        ReasoningEffort::Low => "low".to_owned(),
        ReasoningEffort::Medium => "medium".to_owned(),
        ReasoningEffort::High => "high".to_owned(),
        ReasoningEffort::XHigh => "xhigh".to_owned(),
        ReasoningEffort::Max => "max".to_owned(),
        ReasoningEffort::Ultra => "ultra".to_owned(),
        ReasoningEffort::None => "none".to_owned(),
        ReasoningEffort::TokenBudget(budget) => format!("a {budget}-token budget"),
        _ => "unspecified".to_owned(),
    }
}

/// Stable id of the spawner paragraph.
pub const TOPOLOGY_CONTRIBUTION_ID: &str = "plak.agreements.topology";

/// Stable id of the spawner paragraph in Grok's vocabulary.
pub const TOPOLOGY_GROK_CONTRIBUTION_ID: &str = "plak.agreements.topology-grok";

/// Stable id of the spawner paragraph in pi's vocabulary.
pub const TOPOLOGY_PI_CONTRIBUTION_ID: &str = "plak.agreements.topology-pi";

/// Stable id of the assignee sentence.
pub const ASSIGNMENT_CONTRIBUTION_ID: &str = "plak.agreements.assignment";

/// The tool a node awaits its children with: `wait_agent`, or a pack's own
/// await where it replaces `wait_agent` under the same grant (spec 03 §3).
#[derive(Clone, Copy)]
enum AgentAwait {
    /// `wait_agent`.
    Plak,
    /// Grok's `get_command_or_subagent_output`.
    Grok,
    /// Pi's `wait`.
    Pi,
}

impl AgentAwait {
    /// Earned where `required` and this await are presented: the plain text
    /// on `wait_agent`, a pack text on its own await without `wait_agent`.
    /// Exactly one of the variants is earned on a node.
    fn earned(self, required: &[&str], excluded: &[&str]) -> Earned {
        let names = |names: &[&str]| {
            names
                .iter()
                .filter_map(|name| ToolName::new(*name).ok())
                .collect::<Vec<_>>()
        };
        let mut required = names(required);
        let mut excluded = names(excluded);
        match self {
            Self::Plak => required.extend(names(&["wait_agent"])),
            Self::Grok => {
                required.extend(names(&["get_command_or_subagent_output"]));
                excluded.extend(names(&["wait_agent"]));
            }
            Self::Pi => {
                required.extend(names(&["wait"]));
                excluded.extend(names(&["wait_agent"]));
            }
        }
        if excluded.is_empty() {
            Earned::AllPresented(required)
        } else {
            Earned::PresentedWithout { required, excluded }
        }
    }
}

fn earned_assignment() -> Earned {
    ToolName::new("submit_result").map_or(Earned::Always, Earned::ToolPresented)
}

/// The optional agreement paragraphs as a pack: it presents no tool of its
/// own, and each paragraph states the tool that earns it as an [`Earned`].
///
/// `compile` judges those conditions against the whole callable surface — the
/// presentation's core names plus every product extension, the same set a
/// js-ptc / py-ptc nested catalog lists — so a PTC hub, whose `spawn_agent` is
/// callable only inside a cell, earns the topology paragraph exactly as a
/// Direct hub does. There is no second evaluator: the host
/// never reads tool names to decide a paragraph.
///
/// These are the product's own contributions until the topology pack owns the
/// tools and the paragraphs together: the paragraph that
/// explains `spawn_agent` belongs with `spawn_agent`.
pub struct ProductAgreementPack {
    contributions: Vec<PromptContribution>,
}

impl Default for ProductAgreementPack {
    fn default() -> Self {
        Self::new()
    }
}

impl ProductAgreementPack {
    /// The paragraphs for a node whose host does not wake it.
    #[must_use]
    pub fn new() -> Self {
        Self::with_host_wake(false)
    }

    /// `host_wakes` is the per-node host fact: true when this node's host
    /// starts a new turn on its tracked work (an attached root with
    /// continuation enabled, or any child of the tree host). The tools the
    /// node compiled then decide between root, hub and assignee wording.
    #[must_use]
    pub fn with_host_wake(host_wakes: bool) -> Self {
        let mut contributions = spawner_agreements(host_wakes);
        contributions.push(PromptContribution::new(
            ASSIGNMENT_CONTRIBUTION_ID,
            earned_assignment(),
            PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS,
        ));
        if host_wakes {
            contributions.push(PromptContribution::new(
                "plak.agreements.assignment-wake",
                earned_assignment(),
                PLAK_PRODUCT_ASSIGNMENT_WAKE_AGREEMENTS,
            ));
        }
        contributions.extend(process_agreements(host_wakes));
        Self { contributions }
    }
}

/// The spawner paragraphs: the topology paragraph and the completion channel,
/// each a plain/Grok/pi set earned on the presented agent await, so exactly
/// one of each set renders on a spawner.
fn spawner_agreements(host_wakes: bool) -> Vec<PromptContribution> {
    use AgentAwait::{Grok, Pi, Plak};
    let topology = [
        (
            TOPOLOGY_CONTRIBUTION_ID,
            Plak,
            PLAK_PRODUCT_TOPOLOGY_AGREEMENTS,
        ),
        (
            TOPOLOGY_GROK_CONTRIBUTION_ID,
            Grok,
            PLAK_PRODUCT_TOPOLOGY_GROK_AGREEMENTS,
        ),
        (
            TOPOLOGY_PI_CONTRIBUTION_ID,
            Pi,
            PLAK_PRODUCT_TOPOLOGY_PI_AGREEMENTS,
        ),
    ]
    .map(|(id, await_tool, text)| (id, await_tool.earned(&["spawn_agent"], &[]), text));
    let mut contributions = topology
        .into_iter()
        .map(|(id, earned, text)| PromptContribution::new(id, earned, text))
        .collect::<Vec<_>>();
    let channels: Vec<(&'static str, Earned, &'static str)> = if host_wakes {
        vec![
            (
                "plak.agreements.child-wake",
                Plak.earned(&["spawn_agent"], &["submit_result"]),
                PLAK_PRODUCT_CHILD_WAKE_AGREEMENTS,
            ),
            (
                "plak.agreements.child-wake-grok",
                Grok.earned(&["spawn_agent"], &["submit_result"]),
                PLAK_PRODUCT_CHILD_WAKE_PACK_AGREEMENTS,
            ),
            (
                "plak.agreements.child-wake-pi",
                Pi.earned(&["spawn_agent"], &["submit_result"]),
                PLAK_PRODUCT_CHILD_WAKE_PACK_AGREEMENTS,
            ),
            (
                "plak.agreements.hub-wake",
                Plak.earned(&["spawn_agent", "submit_result"], &[]),
                PLAK_PRODUCT_HUB_WAKE_AGREEMENTS,
            ),
            (
                "plak.agreements.hub-wake-grok",
                Grok.earned(&["spawn_agent", "submit_result"], &[]),
                PLAK_PRODUCT_HUB_WAKE_PACK_AGREEMENTS,
            ),
            (
                "plak.agreements.hub-wake-pi",
                Pi.earned(&["spawn_agent", "submit_result"], &[]),
                PLAK_PRODUCT_HUB_WAKE_PACK_AGREEMENTS,
            ),
        ]
    } else {
        vec![
            (
                "plak.agreements.child-wait",
                Plak.earned(&["spawn_agent"], &[]),
                PLAK_PRODUCT_CHILD_WAIT_AGREEMENTS,
            ),
            (
                "plak.agreements.child-wait-grok",
                Grok.earned(&["spawn_agent"], &[]),
                PLAK_PRODUCT_CHILD_WAIT_GROK_AGREEMENTS,
            ),
            (
                "plak.agreements.child-wait-pi",
                Pi.earned(&["spawn_agent"], &[]),
                PLAK_PRODUCT_CHILD_WAIT_PI_AGREEMENTS,
            ),
        ]
    };
    contributions.extend(
        channels
            .into_iter()
            .map(|(id, earned, text)| PromptContribution::new(id, earned, text)),
    );
    contributions
}

/// One process wait or wake paragraph per presented process vocabulary.
fn process_agreements(host_wakes: bool) -> Vec<PromptContribution> {
    let mut contributions = Vec::new();
    for (tool, wake_id, wait_id, wake_text, wait_text) in [
        (
            "write_stdin",
            "plak.agreements.process-codex-wake",
            "plak.agreements.process-codex-wait",
            PROCESS_CODEX_WAKE_AGREEMENTS,
            PROCESS_CODEX_WAIT_AGREEMENTS,
        ),
        (
            "get_command_or_subagent_output",
            "plak.agreements.process-grok-wake",
            "plak.agreements.process-grok-wait",
            PROCESS_GROK_WAKE_AGREEMENTS,
            PROCESS_GROK_WAIT_AGREEMENTS,
        ),
        (
            "Bash",
            "plak.agreements.process-claude-wake",
            "plak.agreements.process-claude-wait",
            PROCESS_CLAUDE_WAKE_AGREEMENTS,
            PROCESS_CLAUDE_WAIT_AGREEMENTS,
        ),
        (
            "wait",
            "plak.agreements.process-pi-wake",
            "plak.agreements.process-pi-wait",
            PROCESS_PI_WAKE_AGREEMENTS,
            PROCESS_PI_WAIT_AGREEMENTS,
        ),
    ] {
        let (id, text) = if host_wakes {
            (wake_id, wake_text)
        } else {
            (wait_id, wait_text)
        };
        contributions.push(PromptContribution::new(
            id,
            ToolName::new(tool).map_or(Earned::Always, Earned::ToolPresented),
            text,
        ));
    }
    contributions
}

impl ToolPack for ProductAgreementPack {
    fn id(&self) -> &'static str {
        "plak.agreements.v1"
    }

    /// No tool: the paragraphs explain tools other packs present.
    fn menu(&self, _presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        Ok(Vec::new())
    }

    fn contributions(&self) -> &[PromptContribution] {
        &self.contributions
    }

    /// The paragraphs are pinned with the rest of this module's text in
    /// `IDENTITY_PROMPT_PINS` and by every route render.
    fn pins(&self) -> &'static [PromptPin] {
        &[]
    }
}

/// The identity tail, spec 02 §6.2: environment, model, effort, containment,
/// and the commit rule.
///
/// `selected_effort` is the reasoning effort the resolved profile asked for,
/// rendered as its own sentence when there is one. This is Plakous's answer to
/// Codex's turn context: the same facts, in Plakous's words, written once.
#[must_use]
pub fn product_identity_tail(
    workspace_root: &Path,
    cwd: &Path,
    selected_model: &str,
    selected_effort: Option<&str>,
) -> String {
    let mut tail = product_environment_sentence(workspace_root, cwd);
    tail.push('\n');
    tail.push_str(&PLAK_PRODUCT_MODEL_IDENTITY_TEMPLATE.replace("{model}", selected_model));
    if let Some(effort) = selected_effort {
        tail.push('\n');
        tail.push_str(&PLAK_PRODUCT_EFFORT_TEMPLATE.replace("{effort}", effort));
    }
    tail.push('\n');
    tail.push_str(PLAK_PRODUCT_EXECUTION_POLICY);
    tail.push('\n');
    tail.push_str(PLAK_PRODUCT_VCS_POLICY);
    tail
}

/// Environment plus model identity block appended after the family/PTC base.
///
/// The retired two-step order; the pins compare `PromptPlan::assemble` against
/// it so the one-step assembly is proven to write the same bytes.
#[cfg(test)]
#[must_use]
pub fn product_target_identity_block(
    workspace_root: &Path,
    cwd: &Path,
    selected_model: &str,
    selected_effort: Option<&str>,
    contributions: &[PromptContribution],
) -> String {
    let mut block = format!(
        "{}\n\n{}",
        product_identity_tail(workspace_root, cwd, selected_model, selected_effort),
        PLAK_PRODUCT_WORKING_AGREEMENTS
    );
    for contribution in contributions {
        block.push_str("\n\n");
        block.push_str(contribution.text());
    }
    block
}

/// Every model-visible string this module owns.
#[cfg(test)]
pub static IDENTITY_PROMPT_PINS: &[PromptPin] = &[
    PromptPin::new(
        "identity.process_codex_wake_agreements",
        PROCESS_CODEX_WAKE_AGREEMENTS,
        "f8566372e8b254b5123730c00595781442343264c188e1c5b5a3b592e4f2252f",
    ),
    PromptPin::new(
        "identity.process_grok_wake_agreements",
        PROCESS_GROK_WAKE_AGREEMENTS,
        "dd71ab85502dd6a903ab34bd8ab6e75d30aa1f2c77d21a084ac5989c41abfce6",
    ),
    PromptPin::new(
        "identity.process_claude_wake_agreements",
        PROCESS_CLAUDE_WAKE_AGREEMENTS,
        "17025fa103bc79ae65564059c8c864d904062eae132b59f87f178c03ef27811c",
    ),
    PromptPin::new(
        "identity.process_codex_wait_agreements",
        PROCESS_CODEX_WAIT_AGREEMENTS,
        "2e026f7d3d06ebc4d7ca587a5848c57f936fe68796e94d48cda369c140d42322",
    ),
    PromptPin::new(
        "identity.process_grok_wait_agreements",
        PROCESS_GROK_WAIT_AGREEMENTS,
        "dc7cf7506caee6839b8dc27d701eaea4995cf0bbfaa301167ca6519b36a12389",
    ),
    PromptPin::new(
        "identity.process_claude_wait_agreements",
        PROCESS_CLAUDE_WAIT_AGREEMENTS,
        "b434dca041a3fedb35bb4f0b3b8dd1ffa0eefbd98a9d8d19ed78a17a5b2623cd",
    ),
    PromptPin::new(
        "identity.process_pi_wake_agreements",
        PROCESS_PI_WAKE_AGREEMENTS,
        "f1f0072f6395c3a12e1f05729e8c716d42d8ed25b926219dd6dc48c9b71ca26f",
    ),
    PromptPin::new(
        "identity.process_pi_wait_agreements",
        PROCESS_PI_WAIT_AGREEMENTS,
        "3736078cdeede42496ba9044be95859ee87af2917a380755a0abfdec5648fe42",
    ),
    PromptPin::new(
        "identity.product_environment_template",
        PLAK_PRODUCT_ENVIRONMENT_TEMPLATE,
        "3d61e11cfdf329f296b0d4f5f852839f83475c6952d497f83ccbb328567e6679",
    ),
    PromptPin::new(
        "identity.product_model_identity_template",
        PLAK_PRODUCT_MODEL_IDENTITY_TEMPLATE,
        "4a939b1689cbe31ac72cb4b876217eacb4c418add8974d29134dfce0c848155a",
    ),
    PromptPin::new(
        "identity.product_effort_template",
        PLAK_PRODUCT_EFFORT_TEMPLATE,
        "dab64b2dd79b437e84d76942281dc36617a59d31989dc53443f7e0d2577eab88",
    ),
    PromptPin::new(
        "identity.product_execution_policy",
        PLAK_PRODUCT_EXECUTION_POLICY,
        "bf98ce0dacbd179be05381ca734fcfd6465fefcab9e2504ac373055d352bcd34",
    ),
    PromptPin::new(
        "identity.product_vcs_policy",
        PLAK_PRODUCT_VCS_POLICY,
        "c15e58dceddec14657265bc5e4dd1da3e83851e2140e8885b768107c99acc2b1",
    ),
    PromptPin::new(
        "identity.product_working_agreements",
        PLAK_PRODUCT_WORKING_AGREEMENTS,
        "76efc1fa953e4b4115e756bf19a581de31a2006f9c3711271450804faa8df212",
    ),
    PromptPin::new(
        "identity.product_topology_agreements",
        PLAK_PRODUCT_TOPOLOGY_AGREEMENTS,
        "f52754045e2857fc6c416ea20146770446e649ef726defc3454ec689008196c2",
    ),
    PromptPin::new(
        "identity.product_topology_grok_agreements",
        PLAK_PRODUCT_TOPOLOGY_GROK_AGREEMENTS,
        "6b423e262efd27ab5fac38978a4e3e949c29e437fa921c8d192793984c9ddf49",
    ),
    PromptPin::new(
        "identity.product_topology_pi_agreements",
        PLAK_PRODUCT_TOPOLOGY_PI_AGREEMENTS,
        "2877e9469ec48a23c42fe48973e5a85a51b818eabdbde92575a17e7d1d8e860e",
    ),
    PromptPin::new(
        "identity.product_child_wake_agreements",
        PLAK_PRODUCT_CHILD_WAKE_AGREEMENTS,
        "9dfcfe8e84c287bc6d05075fe3a3cb487cb4d3c91bf246c964ccdd73200805b9",
    ),
    PromptPin::new(
        "identity.product_child_wake_pack_agreements",
        PLAK_PRODUCT_CHILD_WAKE_PACK_AGREEMENTS,
        "1c5a79a79ee0c6dd7736423ddd6d4f33c3f3275720f61b19fb790729cbd69a64",
    ),
    PromptPin::new(
        "identity.product_hub_wake_pack_agreements",
        PLAK_PRODUCT_HUB_WAKE_PACK_AGREEMENTS,
        "416e3cb1cb2a726373d10c8b47562a4fd67a66b8d753f1e198dcfbd32e54e87d",
    ),
    PromptPin::new(
        "identity.product_hub_wake_agreements",
        PLAK_PRODUCT_HUB_WAKE_AGREEMENTS,
        "274f91f3a27a26814be9f2e4c4a9ffb3c7de0e8fae2f45eb3938bad090255f70",
    ),
    PromptPin::new(
        "identity.product_assignment_wake_agreements",
        PLAK_PRODUCT_ASSIGNMENT_WAKE_AGREEMENTS,
        "e76709f089bb4365c31adb6d3358387795f7ea26b407d1009e2a2f3083d4dbfb",
    ),
    PromptPin::new(
        "identity.product_child_wait_agreements",
        PLAK_PRODUCT_CHILD_WAIT_AGREEMENTS,
        "75909c1ac5a3d4bd2cf0140f28c334a8be578173fb0b5148e3edb6e5b860af83",
    ),
    PromptPin::new(
        "identity.product_child_wait_grok_agreements",
        PLAK_PRODUCT_CHILD_WAIT_GROK_AGREEMENTS,
        "e60ed0218615ea500fefd064d24ca0e87d53eccf3a9e1e5b4a9eab52c195b213",
    ),
    PromptPin::new(
        "identity.product_child_wait_pi_agreements",
        PLAK_PRODUCT_CHILD_WAIT_PI_AGREEMENTS,
        "267d0c11559f1832279453d9f06ed966bf8324626af130023bba497a5e27af4b",
    ),
    PromptPin::new(
        "identity.product_assignment_agreements",
        PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS,
        "63f163bb37606ed7aefb11398ad431db0ed56babb4c3516eb7ed7d94c5410a97",
    ),
];

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests {
    use super::*;
    use crate::surface_fixture::{earned_contributions, surface};
    use std::path::Path;

    #[test]
    fn product_environment_template_is_stable() {
        assert!(PLAK_PRODUCT_ENVIRONMENT_TEMPLATE.starts_with("Workspace root:"));
    }

    #[test]
    fn product_environment_sentence_uses_paths_and_os() {
        let sentence = product_environment_sentence(Path::new("/repo"), Path::new("/repo/src"));
        assert!(sentence.contains("Workspace root: /repo"));
        assert!(sentence.contains("cwd: /repo/src"));
        assert!(sentence.contains(&format!("OS: {}.", std::env::consts::OS)));
        assert!(!sentence.starts_with(' '));
    }

    #[test]
    fn product_model_identity_template_is_stable() {
        assert!(PLAK_PRODUCT_MODEL_IDENTITY_TEMPLATE.starts_with("You are {model}"));
    }

    #[test]
    fn product_target_identity_block_joins_environment_model_and_vcs() {
        let leaf = product_target_identity_block(
            Path::new("/repo"),
            Path::new("/repo/src"),
            "gpt-5",
            Some("high"),
            &earned_contributions(CodingPresentation::Codex, &["submit_result"]),
        );
        assert!(leaf.starts_with("Workspace root: /repo"));
        assert!(leaf.contains("\nYou are gpt-5."));
        assert!(leaf.contains("This turn's prompts/instructions identify you"));
        assert!(leaf.contains(PLAK_PRODUCT_VCS_POLICY));
        assert!(leaf.contains(PLAK_PRODUCT_WORKING_AGREEMENTS));
        assert_eq!(leaf.matches(PLAK_PRODUCT_VCS_POLICY).count(), 1);
        assert!(!PLAK_PRODUCT_WORKING_AGREEMENTS.contains("Workspace root:"));
        assert!(!PLAK_PRODUCT_WORKING_AGREEMENTS.contains("Commit or push"));
        assert!(!PLAK_PRODUCT_WORKING_AGREEMENTS.contains("task_apply"));
        assert!(!PLAK_PRODUCT_WORKING_AGREEMENTS.contains("lead with the answer"));
        assert!(
            PLAK_PRODUCT_WORKING_AGREEMENTS
                .contains("You may call independent presented tools together")
        );
    }

    /// Pi's process guidance is earned by the presented `wait`, and only a
    /// node whose host wakes it is told to end the turn.
    #[test]
    fn pi_wait_guidance_is_keyed_on_the_presented_wait() {
        for host_wakes in [false, true] {
            let earned = |tools: &[&str]| {
                let names = tools
                    .iter()
                    .map(|name| ToolName::new(*name).expect("tool"))
                    .collect();
                ProductAgreementPack::with_host_wake(host_wakes)
                    .contributions()
                    .iter()
                    .filter(|fragment| fragment.earned().is_earned_by(&names))
                    .map(PromptContribution::text)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let text = earned(&["bash", "wait"]);
            assert!(text.contains("Background jobs report automatically"));
            assert!(text.contains("Never poll or sleep"));
            assert!(text.contains("Use wait"));
            assert_eq!(text.contains("end this turn"), host_wakes);
            assert!(!earned(&["bash"]).contains("Use wait"));
        }
    }

    /// a leaf receives no topology paragraph at all, while the
    /// spawner paragraph and the assignee sentence are earned separately. The
    /// names are judged by `compile` over the whole callable surface.
    #[test]
    fn process_wait_guidance_matches_the_host_and_presented_vocabulary() {
        for host_wakes in [false, true] {
            for assignee in [false, true] {
                for (tool, in_turn_wait) in [
                    ("write_stdin", "Use write_stdin to wait"),
                    (
                        "get_command_or_subagent_output",
                        "Use get_command_or_subagent_output",
                    ),
                    ("Bash", "Bash"),
                ] {
                    let mut names = vec![ToolName::new(tool).expect("wait tool")];
                    if assignee {
                        names.push(ToolName::new("submit_result").expect("assignment"));
                    }
                    let names = names.into_iter().collect();
                    let text = ProductAgreementPack::with_host_wake(host_wakes)
                        .contributions()
                        .iter()
                        .filter(|fragment| fragment.earned().is_earned_by(&names))
                        .map(PromptContribution::text)
                        .collect::<Vec<_>>()
                        .join("\n");
                    // The host fact, not the node's role, decides the channel:
                    // the tree host wakes assignees as an attached host wakes
                    // its root.
                    assert_eq!(
                        text.contains("When only waiting remains, end this turn"),
                        host_wakes
                    );
                    assert!(text.contains(in_turn_wait));
                    assert_eq!(
                        text.contains("only when you need the output to continue this turn")
                            || text.contains("only when you need a result to continue this turn"),
                        host_wakes
                    );
                    assert_eq!(
                        text.contains(PLAK_PRODUCT_ASSIGNMENT_WAKE_AGREEMENTS),
                        host_wakes && assignee
                    );
                    assert!(text.contains("sleep-poll") || text.contains("do not poll"));
                    assert!(text.contains("servers"));
                }
            }
        }
        // Claude's wording follows Claude Code's background-task guidance.
        assert!(PROCESS_CLAUDE_WAKE_AGREEMENTS.contains("you'll be notified when"));
        assert!(PROCESS_CLAUDE_WAKE_AGREEMENTS.contains("do not poll"));
    }

    #[test]
    fn wake_completion_channel_follows_the_host_fact_and_role() {
        for host_wakes in [false, true] {
            let pack = ProductAgreementPack::with_host_wake(host_wakes);
            for (tools, root) in [
                (vec!["spawn_agent", "wait_agent"], true),
                (vec!["spawn_agent", "submit_result", "wait_agent"], false),
                (vec!["submit_result"], false),
            ] {
                let names = tools
                    .iter()
                    .map(|name| ToolName::new(*name).expect("fixture name"))
                    .collect();
                let text = pack
                    .contributions()
                    .iter()
                    .filter(|contribution| contribution.earned().is_earned_by(&names))
                    .map(PromptContribution::text)
                    .collect::<Vec<_>>()
                    .join("\n");
                let spawner = tools.contains(&"spawn_agent");
                assert_eq!(
                    text.contains(PLAK_PRODUCT_CHILD_WAKE_AGREEMENTS),
                    host_wakes && root
                );
                assert_eq!(
                    text.contains(PLAK_PRODUCT_HUB_WAKE_AGREEMENTS),
                    host_wakes && spawner && !root
                );
                assert_eq!(
                    text.contains(PLAK_PRODUCT_CHILD_WAIT_AGREEMENTS),
                    spawner && !host_wakes
                );
                assert!(!text.contains("get_command_or_subagent_output"));
            }
            // A Grok spawner without a waking host learns of results through
            // the output tool it was given, never through wait_agent.
            let names = ["spawn_agent", "get_command_or_subagent_output"]
                .iter()
                .map(|name| ToolName::new(*name).expect("fixture name"))
                .collect();
            let text = pack
                .contributions()
                .iter()
                .filter(|contribution| contribution.earned().is_earned_by(&names))
                .map(PromptContribution::text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains(PLAK_PRODUCT_TOPOLOGY_GROK_AGREEMENTS));
            assert!(!text.contains(PLAK_PRODUCT_TOPOLOGY_AGREEMENTS));
            assert_eq!(
                text.contains(PLAK_PRODUCT_CHILD_WAIT_GROK_AGREEMENTS),
                !host_wakes
            );
            assert!(!text.contains(PLAK_PRODUCT_CHILD_WAIT_AGREEMENTS));
        }
    }

    /// Where the Grok output await is the agent await (no `wait_agent`), the
    /// wake paragraphs name it instead; a surface with `wait_agent` keeps the
    /// `wait_agent` wording even beside the Grok process tools.
    #[test]
    fn grok_wake_paragraphs_name_the_output_await_when_wait_agent_is_absent() {
        let pack = ProductAgreementPack::with_host_wake(true);
        let earned = |names: &[&str]| {
            let names = names
                .iter()
                .map(|name| ToolName::new(*name).expect("fixture name"))
                .collect();
            pack.contributions()
                .iter()
                .filter(|contribution| contribution.earned().is_earned_by(&names))
                .map(PromptContribution::id)
                .collect::<Vec<_>>()
        };
        let grok = "get_command_or_subagent_output";
        assert!(earned(&["spawn_agent", grok]).contains(&"plak.agreements.child-wake-grok"));
        assert!(!earned(&["spawn_agent", grok]).contains(&"plak.agreements.child-wake"));
        let hub = earned(&["spawn_agent", "submit_result", grok]);
        assert!(hub.contains(&"plak.agreements.hub-wake-grok"));
        assert!(!hub.contains(&"plak.agreements.hub-wake"));
        let today = earned(&["spawn_agent", "submit_result", "wait_agent", grok]);
        assert!(today.contains(&"plak.agreements.hub-wake"));
        assert!(!today.contains(&"plak.agreements.hub-wake-grok"));
        let root = earned(&["spawn_agent", "wait_agent", grok]);
        assert!(root.contains(&"plak.agreements.child-wake"));
        assert!(!root.contains(&"plak.agreements.child-wake-grok"));
        for text in [
            PLAK_PRODUCT_CHILD_WAKE_PACK_AGREEMENTS,
            PLAK_PRODUCT_HUB_WAKE_PACK_AGREEMENTS,
        ] {
            assert!(
                !text.contains("only when"),
                "the process paragraph is the one only-when sentence: {text}"
            );
            assert!(!text.contains("wait_agent"));
        }
        assert!(PROCESS_GROK_WAKE_AGREEMENTS.contains(
            "Use get_command_or_subagent_output with nonzero timeout_ms only when you need a \
             result to continue this turn."
        ));
    }

    /// Where pi's `wait` is the agent await (no `wait_agent`), exactly one of
    /// each spawner set is earned and it is the pi one; a surface with
    /// `wait_agent` keeps the plain wording even beside pi's process `wait`.
    #[test]
    fn pi_spawner_paragraphs_name_wait_when_wait_agent_is_absent() {
        for host_wakes in [false, true] {
            let pack = ProductAgreementPack::with_host_wake(host_wakes);
            let earned = |names: &[&str]| {
                let names = names
                    .iter()
                    .map(|name| ToolName::new(*name).expect("fixture name"))
                    .collect();
                pack.contributions()
                    .iter()
                    .filter(|contribution| contribution.earned().is_earned_by(&names))
                    .map(PromptContribution::id)
                    .collect::<Vec<_>>()
            };
            let spawner_ids = |ids: &[&'static str]| {
                ids.iter()
                    .copied()
                    .filter(|id| {
                        id.starts_with("plak.agreements.topology")
                            || id.starts_with("plak.agreements.child-")
                            || id.starts_with("plak.agreements.hub-")
                    })
                    .collect::<Vec<_>>()
            };
            let root = spawner_ids(&earned(&["spawn_agent", "wait", "stop"]));
            let hub = spawner_ids(&earned(&["spawn_agent", "submit_result", "wait", "stop"]));
            if host_wakes {
                assert_eq!(
                    root,
                    [TOPOLOGY_PI_CONTRIBUTION_ID, "plak.agreements.child-wake-pi"]
                );
                assert_eq!(
                    hub,
                    [TOPOLOGY_PI_CONTRIBUTION_ID, "plak.agreements.hub-wake-pi"]
                );
            } else {
                assert_eq!(
                    root,
                    [TOPOLOGY_PI_CONTRIBUTION_ID, "plak.agreements.child-wait-pi"]
                );
            }
            let plain = spawner_ids(&earned(&["spawn_agent", "wait_agent", "wait"]));
            assert!(plain.iter().all(|id| !id.ends_with("-pi")), "{plain:?}");
            assert!(plain.contains(&TOPOLOGY_CONTRIBUTION_ID));
        }
        for text in [
            PLAK_PRODUCT_TOPOLOGY_PI_AGREEMENTS,
            PLAK_PRODUCT_CHILD_WAIT_PI_AGREEMENTS,
        ] {
            assert!(text.contains("wait"));
            assert!(!text.contains("wait_agent"), "{text}");
            assert!(!text.contains("interrupt_agent"), "{text}");
        }
        for text in [
            PLAK_PRODUCT_CHILD_WAKE_PACK_AGREEMENTS,
            PLAK_PRODUCT_HUB_WAKE_PACK_AGREEMENTS,
        ] {
            assert!(
                !text.contains("only when"),
                "the process paragraph is the one only-when sentence: {text}"
            );
            assert!(!text.contains("wait_agent"), "{text}");
        }
        assert!(
            PROCESS_PI_WAKE_AGREEMENTS
                .contains("Use wait only when you need a result to continue this turn.")
        );
    }

    #[test]
    fn topology_paragraph_is_earned_by_the_compiled_tool_names() {
        let block = |names: &[&str]| {
            product_target_identity_block(
                Path::new("/repo"),
                Path::new("/repo/src"),
                "gpt-5",
                Some("high"),
                &earned_contributions(CodingPresentation::Codex, names),
            )
        };
        let single_agent = block(&[]);
        assert!(!single_agent.contains(PLAK_PRODUCT_TOPOLOGY_AGREEMENTS));
        assert!(!single_agent.contains(PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS));
        assert!(!single_agent.contains("wait_agent"));
        assert!(!single_agent.contains("submit_result"));

        let leaf = block(&["submit_result", "list_agents"]);
        assert!(!leaf.contains(PLAK_PRODUCT_TOPOLOGY_AGREEMENTS));
        assert!(!leaf.contains("wait_agent"));
        assert!(!leaf.contains("adopt_candidate"));
        assert!(!leaf.contains("spawn_agent"));
        assert!(leaf.contains(PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS));

        let hub = block(&["spawn_agent", "wait_agent", "submit_result"]);
        assert!(hub.contains(PLAK_PRODUCT_TOPOLOGY_AGREEMENTS));
        assert!(hub.contains(PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS));
        assert!(
            hub.find(PLAK_PRODUCT_TOPOLOGY_AGREEMENTS)
                < hub.find(PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS)
        );

        let root = block(&["spawn_agent", "wait_agent"]);
        assert!(root.contains(PLAK_PRODUCT_TOPOLOGY_AGREEMENTS));
        assert!(!root.contains(PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS));
    }

    /// The pack presents nothing and earns through `compile` alone: a surface
    /// whose extensions are callable only nested still earns both paragraphs.
    #[test]
    fn agreement_pack_presents_no_tool_and_earns_through_the_surface() {
        let pack = ProductAgreementPack::new();
        assert!(
            pack.menu(CodingPresentation::Codex)
                .expect("menu")
                .is_empty()
        );
        assert_eq!(
            pack.contributions()
                .iter()
                .map(PromptContribution::id)
                .collect::<Vec<_>>(),
            [
                TOPOLOGY_CONTRIBUTION_ID,
                TOPOLOGY_GROK_CONTRIBUTION_ID,
                TOPOLOGY_PI_CONTRIBUTION_ID,
                "plak.agreements.child-wait",
                "plak.agreements.child-wait-grok",
                "plak.agreements.child-wait-pi",
                ASSIGNMENT_CONTRIBUTION_ID,
                "plak.agreements.process-codex-wait",
                "plak.agreements.process-grok-wait",
                "plak.agreements.process-claude-wait",
                "plak.agreements.process-pi-wait",
            ]
        );
        assert_eq!(
            ProductAgreementPack::with_host_wake(true)
                .contributions()
                .iter()
                .map(PromptContribution::id)
                .collect::<Vec<_>>(),
            [
                TOPOLOGY_CONTRIBUTION_ID,
                TOPOLOGY_GROK_CONTRIBUTION_ID,
                TOPOLOGY_PI_CONTRIBUTION_ID,
                "plak.agreements.child-wake",
                "plak.agreements.child-wake-grok",
                "plak.agreements.child-wake-pi",
                "plak.agreements.hub-wake",
                "plak.agreements.hub-wake-grok",
                "plak.agreements.hub-wake-pi",
                ASSIGNMENT_CONTRIBUTION_ID,
                "plak.agreements.assignment-wake",
                "plak.agreements.process-codex-wake",
                "plak.agreements.process-grok-wake",
                "plak.agreements.process-claude-wake",
                "plak.agreements.process-pi-wake",
            ]
        );
        // A Grok hub presents its await as get_command_or_subagent_output, not
        // wait_agent, and earns the texts that name it.
        let hub = surface(CodingPresentation::Grok, &["spawn_agent", "submit_result"]);
        assert!(
            hub.exposed_names()
                .iter()
                .any(|name| name.as_str() == "spawn_agent")
        );
        assert_eq!(
            hub.contributions()
                .iter()
                .map(PromptContribution::id)
                .collect::<Vec<_>>(),
            [
                TOPOLOGY_GROK_CONTRIBUTION_ID,
                "plak.agreements.child-wait-grok",
                ASSIGNMENT_CONTRIBUTION_ID,
                "plak.agreements.process-grok-wait",
            ]
        );
    }

    #[test]
    fn product_working_agreements_are_stable() {
        assert!(PLAK_PRODUCT_WORKING_AGREEMENTS.starts_with("Work from tool results:"));
        assert!(
            PLAK_PRODUCT_WORKING_AGREEMENTS
                .contains("Do not retry a deterministic invalid call with the same arguments")
        );
        assert!(PLAK_PRODUCT_TOPOLOGY_AGREEMENTS.contains("Default: children share your checkout"));
        for retired in [
            "merge_workspace",
            "write_effect",
            "workspace_mode",
            "immediately",
            "until it settles",
            "does not merge worktrees",
            "rejects mail",
        ] {
            for text in [
                PLAK_PRODUCT_WORKING_AGREEMENTS,
                PLAK_PRODUCT_TOPOLOGY_AGREEMENTS,
                PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS,
            ] {
                assert!(!text.contains(retired), "{retired} in {text}");
            }
        }
    }
}
