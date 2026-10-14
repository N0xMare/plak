//! Child assignment Start prompts and submit harness nudges.

#[cfg(test)]
use plakous::tools::PromptPin;

use plakous::prompts::vocabulary::{AgentTier, Composition};

/// Which child Start template to use from a resolved composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildAssignmentStartKind {
    /// `submit_result` is a presented tool the child calls directly.
    Direct,
    /// `submit_result` is reached as `tools.submit_result` inside a cell.
    ToolsPtc,
    /// `submit_result` is reached as a bare call inside a Python cell.
    PyPtc,
}

impl ChildAssignmentStartKind {
    /// The Start template one composition's children are given.
    #[must_use]
    pub const fn from_composition(composition: Composition) -> Self {
        match composition {
            Composition::PythonCell => Self::PyPtc,
            Composition::NativeCell | Composition::JavaScriptCell => Self::ToolsPtc,
            Composition::Direct => Self::Direct,
        }
    }
}

/// Direct child genesis Start; `{template}`, `{tier}`, `{description}`, `{prompt}`, `{turn_token}`, `{schema}`, `{workspace_line}`, `{tier_line}`.
pub const PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_DIRECT: &str = "Template: {template}\nTier: {tier}\n{workspace_line}Description: {description}\nPrompt: {prompt}\nTurn token: {turn_token}\n\nAssignment output schema:\n{schema}\n\nCall submit_result with this turn_token and schema-valid output_json, then stop; retry only if the tool result says the draft was rejected; after a committed receipt, do not submit another result. Do not answer the assignment in assistant text. Ending a turn without submit_result while nothing you started is running ends as completed_without_submission after reminders, not a successful assignment; do not treat the last assistant text as a result. Native plan or todo updates are not submission. {tier_line}";

/// native-ptc / js-ptc child genesis Start.
pub const PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_TOOLS_PTC: &str = "The only success path is await tools.submit_result({turn_token: {turn_token}, output_json: \"<JSON string>\"}) matching the schema below; do not answer in assistant text.\n\nTemplate: {template}\nTier: {tier}\n{workspace_line}Description: {description}\nPrompt: {prompt}\nTurn token: {turn_token}\n\nAssignment output schema:\n{schema}\n\nEnding a turn without submit_result while nothing you started is running ends as completed_without_submission after reminders; do not treat assistant text as a result. Native plan or todo updates are not submission. {tier_line}";

/// py-ptc child genesis Start.
pub const PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_PY_PTC: &str = "The only success path is await submit_result({turn_token: {turn_token}, output_json: \"<JSON string>\"}) matching the schema below; do not answer in assistant text.\n\nTemplate: {template}\nTier: {tier}\n{workspace_line}Description: {description}\nPrompt: {prompt}\nTurn token: {turn_token}\n\nAssignment output schema:\n{schema}\n\nEnding a turn without submit_result while nothing you started is running ends as completed_without_submission after reminders; do not treat assistant text as a result. Native plan or todo updates are not submission. {tier_line}";

/// Prefix for the workspace line when the child owns its own copy (snapshot or fork); `{path}` is the POSIX path.
pub const PLAK_CHILD_ASSIGNMENT_WORKSPACE_LINE_TEMPLATE: &str = "Workspace: {path}\n";

/// `read_only` inherit closer: shared checkout, shell allowed, no file writes.
pub const PLAK_CHILD_TIER_LINE_READ_ONLY: &str = "Your tier is read_only: you share the parent's live checkout. Process tools are allowed. File-writing tools are not presented; report rather than edit. Plan tools stay allowed when presented. Other agents may be working in this checkout; treat concurrent changes as owned by their authors and do not overwrite them. Use list_agents and send_agent_message to coordinate. adopt_candidate and discard_workspace stay valid when presented.";

/// `read_only` isolated closer: own snapshot copy.
pub const PLAK_CHILD_TIER_LINE_READ_ONLY_ISOLATED: &str = "Your tier is read_only: the workspace above is your own snapshot copy of the parent's state. Process tools are allowed. File-writing tools are not presented; report rather than edit. Plan tools stay allowed when presented. The copy is thrown away unless the parent adopts a candidate sealed from it.";

/// `execute` inherit closer.
pub const PLAK_CHILD_TIER_LINE_EXECUTE: &str = "Your tier is execute: you share the parent's live checkout. Process tools are allowed. File-writing tools are not presented; plan tools stay allowed when presented. Other agents may be working here; treat concurrent changes as owned by their authors and do not overwrite them.";

/// `mutate` inherit closer.
pub const PLAK_CHILD_TIER_LINE_MUTATE: &str = "Your tier is mutate: you share the parent's live checkout. File and process tools are allowed. Other agents may be working here; give them file-disjoint work or run writers in sequence. Treat concurrent changes as owned by their authors and do not overwrite them.";

/// `execute` isolated closer: own snapshot copy.
pub const PLAK_CHILD_TIER_LINE_EXECUTE_ISOLATED: &str = "Your tier is execute: the workspace above is your own snapshot copy of the parent's state. Process tools are allowed. File-writing tools are not presented; plan tools stay allowed when presented. The copy is thrown away unless the parent adopts a candidate sealed from it.";

/// `mutate` isolated closer: own fork.
pub const PLAK_CHILD_TIER_LINE_MUTATE_ISOLATED: &str = "Your tier is mutate: the workspace above is your own fork of the parent's state. File and process tools are allowed, and nothing you write reaches the parent until it adopts a candidate sealed from this fork.";

/// The one tier line a child's Start prompt closes with.
#[must_use]
pub const fn child_tier_line(tier: AgentTier, isolated: bool) -> &'static str {
    match (tier, isolated) {
        (AgentTier::ReadOnly, false) => PLAK_CHILD_TIER_LINE_READ_ONLY,
        (AgentTier::ReadOnly, true) => PLAK_CHILD_TIER_LINE_READ_ONLY_ISOLATED,
        (AgentTier::Execute, false) => PLAK_CHILD_TIER_LINE_EXECUTE,
        (AgentTier::Execute, true) => PLAK_CHILD_TIER_LINE_EXECUTE_ISOLATED,
        (AgentTier::Mutate, false) => PLAK_CHILD_TIER_LINE_MUTATE,
        (AgentTier::Mutate, true) => PLAK_CHILD_TIER_LINE_MUTATE_ISOLATED,
    }
}

/// Prefix appended when a Start prompt includes a mail body after the assignment brief.
pub const PLAK_CHILD_ASSIGNMENT_MESSAGE_PREFIX: &str = "\n\nMessage:\n";

/// First submit harness nudge after completed_without_submission; `{turn_token}`, `{schema}`.
pub const PLAK_CHILD_SUBMIT_NUDGE_REMINDER_TEMPLATE: &str = "Your previous turn completed without calling submit_result. Assignment output is not the last assistant text. Call submit_result exactly once (directly if it is a top-level tool; otherwise as a nested call inside a cell, the only callable path for nested tools) with turn_token={turn_token} and output_json matching this schema, then stop.\n\nAssignment output schema:\n{schema}";

/// Second submit harness nudge forcing submit_result; `{turn_token}`, `{schema}`.
pub const PLAK_CHILD_SUBMIT_NUDGE_FORCE_TEMPLATE: &str = "Required: call submit_result now (top-level tool, or nested inside a cell when that is the only callable path) with turn_token={turn_token} and schema-valid output_json. Do not write a final assistant answer. Ending the turn without submit_result while nothing you started is running is completed_without_submission.\n\nAssignment output schema:\n{schema}";

/// The fixed instruction of a host wake turn on a parked child. Process
/// notices and the coalesced mail batch follow it.
pub const PLAK_CHILD_WAKE_PROMPT: &str = "Work you were waiting on changed while you were idle. Continue the assignment; call submit_result when it is done, or end the turn if only waiting remains.";

/// The Start template for one child composition.
#[must_use]
pub fn child_assignment_start_template(kind: ChildAssignmentStartKind) -> &'static str {
    match kind {
        ChildAssignmentStartKind::Direct => PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_DIRECT,
        ChildAssignmentStartKind::ToolsPtc => PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_TOOLS_PTC,
        ChildAssignmentStartKind::PyPtc => PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_PY_PTC,
    }
}

/// One child's genesis Start prompt, rendered from its parts.
///
/// The substitution list used to live at the assembly site as an eight-call
/// `replace` chain, so a missing field was a `{placeholder}` in a model-visible
/// prompt. Naming the parts makes the template's contract checkable.
#[derive(Clone, Copy, Debug)]
pub struct ChildAssignmentStart<'a> {
    /// Which composition's Start template to render.
    pub kind: ChildAssignmentStartKind,
    /// The template card name the child was spawned from.
    pub template: &'a str,
    /// Capability ceiling for the closer; isolation comes from `workspace_path`.
    pub tier: AgentTier,
    /// The spawn description; the template name stands in when it is empty.
    pub description: &'a str,
    /// The assignment prompt.
    pub prompt: &'a str,
    /// The assignment output schema, rendered verbatim.
    pub schema: &'a str,
    /// The token `submit_result` must echo.
    pub turn_token: u64,
    /// The child's own workspace copy, when it has one.
    pub workspace_path: Option<&'a str>,
    /// A mail body delivered with the assignment.
    pub message: Option<&'a str>,
}

impl ChildAssignmentStart<'_> {
    /// Renders the Start prompt.
    #[must_use]
    pub fn render(&self) -> String {
        let description = if self.description.is_empty() {
            self.template
        } else {
            self.description
        };
        let workspace_line = self
            .workspace_path
            .map(|path| PLAK_CHILD_ASSIGNMENT_WORKSPACE_LINE_TEMPLATE.replace("{path}", path))
            .unwrap_or_default();
        let mut body = child_assignment_start_template(self.kind)
            .replace("{template}", self.template)
            .replace("{tier}", self.tier.as_str())
            .replace("{workspace_line}", &workspace_line)
            .replace(
                "{tier_line}",
                child_tier_line(self.tier, self.workspace_path.is_some()),
            )
            .replace("{description}", description)
            .replace("{prompt}", self.prompt)
            .replace("{turn_token}", &self.turn_token.to_string())
            .replace("{schema}", self.schema);
        if let Some(mail) = self.message {
            body.push_str(PLAK_CHILD_ASSIGNMENT_MESSAGE_PREFIX);
            body.push_str(mail);
        }
        body
    }
}

/// The two submit nudges, in escalation order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitNudge {
    /// First nudge after a turn that completed without `submit_result`.
    Reminder,
    /// Second nudge: no final assistant answer, submit now.
    Force,
}

impl SubmitNudge {
    /// The nudge for one escalation count; anything past the first forces.
    #[must_use]
    pub const fn after(nudge: u8) -> Self {
        if nudge <= 1 {
            Self::Reminder
        } else {
            Self::Force
        }
    }

    /// The template this nudge renders.
    #[must_use]
    pub const fn template(self) -> &'static str {
        match self {
            Self::Reminder => PLAK_CHILD_SUBMIT_NUDGE_REMINDER_TEMPLATE,
            Self::Force => PLAK_CHILD_SUBMIT_NUDGE_FORCE_TEMPLATE,
        }
    }

    /// Renders the nudge for one turn token and assignment schema.
    #[must_use]
    pub fn render(self, turn_token: u64, schema: &str) -> String {
        self.template()
            .replace("{turn_token}", &turn_token.to_string())
            .replace("{schema}", schema)
    }
}

/// Every model-visible string this module owns.
#[cfg(test)]
pub static CHILD_PROMPT_PINS: &[PromptPin] = &[
    PromptPin::new(
        "child.child_transient_resume_template",
        PLAK_CHILD_TRANSIENT_RESUME_TEMPLATE,
        "8e39e6d541832bf12e2fa148938390a1ccf1e3069d0d6b37ccc7212be0134aa5",
    ),
    PromptPin::new(
        "child.child_assignment_start_template_direct",
        PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_DIRECT,
        "cc2e1169792c46cfadc9b2514ab2077fe65cd624e50878da345d39950fe8a65d",
    ),
    PromptPin::new(
        "child.child_assignment_start_template_tools_ptc",
        PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_TOOLS_PTC,
        "974df5277639c4d6e5d24372fcec87f5ce0f40964fcd9c11dbe7cf9762bde2eb",
    ),
    PromptPin::new(
        "child.child_assignment_start_template_py_ptc",
        PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_PY_PTC,
        "5c8d58cd147bc85b9cbfe740fca6c5b8a3444c8be80c0292cd5cf0897b9236b7",
    ),
    PromptPin::new(
        "child.child_assignment_workspace_line_template",
        PLAK_CHILD_ASSIGNMENT_WORKSPACE_LINE_TEMPLATE,
        "b7dab81dbc5f8f5a4ebcb92b64d6caa9cdd14b46a8eaa996db53d129d90b589b",
    ),
    PromptPin::new(
        "child.child_assignment_message_prefix",
        PLAK_CHILD_ASSIGNMENT_MESSAGE_PREFIX,
        "e0ed2e668d4132cf46751fec30bf8e740f16f57c56010cd9bf2ca7a3e5099ddf",
    ),
    PromptPin::new(
        "child.child_tier_line_read_only",
        PLAK_CHILD_TIER_LINE_READ_ONLY,
        "c90cc01f2b87dd82b4b8d4ab85f982290022677e59ca0d919111f1868f07ebc0",
    ),
    PromptPin::new(
        "child.child_tier_line_read_only_isolated",
        PLAK_CHILD_TIER_LINE_READ_ONLY_ISOLATED,
        "181f7da4b47f13d3ce3086f1eefc567590abb36a7b9e460eec7d152184878d14",
    ),
    PromptPin::new(
        "child.child_tier_line_execute",
        PLAK_CHILD_TIER_LINE_EXECUTE,
        "871509e46125a25148825a4eec3757816829f9bd453b5f684d0f6e97433064d8",
    ),
    PromptPin::new(
        "child.child_tier_line_mutate",
        PLAK_CHILD_TIER_LINE_MUTATE,
        "2a1d439d1cc27ea374f46b1b9fdc7c016555e4a7a3f2945afd8745d91f4b4b98",
    ),
    PromptPin::new(
        "child.child_tier_line_execute_isolated",
        PLAK_CHILD_TIER_LINE_EXECUTE_ISOLATED,
        "ca0516054794c65e6b59a8032300ce4f30d6a5d74e09fdb1f685704b03233de2",
    ),
    PromptPin::new(
        "child.child_tier_line_mutate_isolated",
        PLAK_CHILD_TIER_LINE_MUTATE_ISOLATED,
        "11645d1bccb2cc90a3471013daa4ad33f0c516949b7b75ea260abca9d927dc9d",
    ),
    PromptPin::new(
        "child.child_wake_prompt",
        PLAK_CHILD_WAKE_PROMPT,
        "17fff190a809e6021aa75ce1377b93911bc7737fb98be160480ad4168fc1c91b",
    ),
    PromptPin::new(
        "child.child_submit_nudge_reminder_template",
        PLAK_CHILD_SUBMIT_NUDGE_REMINDER_TEMPLATE,
        "d90c013245b68a34193468369ebc903b876bd65fdcd6f8a062b15fa46c94d416",
    ),
    PromptPin::new(
        "child.child_submit_nudge_force_template",
        PLAK_CHILD_SUBMIT_NUDGE_FORCE_TEMPLATE,
        "0347ff2ea96034f5ea3c1c23f80165b5155d17559adbb5f3e2f3a7bb32715085",
    ),
];

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests {
    #[test]
    fn child_prompt_pins_match_unchanged_text() {
        assert!(plakous::tools::drifted_pins(CHILD_PROMPT_PINS).is_empty());
    }
    #[test]
    fn transient_resume_render_preserves_failure_and_does_not_replay_work() {
        assert!(
            CHILD_PROMPT_PINS
                .iter()
                .any(|pin| pin.id() == "child.child_transient_resume_template")
        );
        let rendered = TransientResume {
            failure: "DNS lookup failed before anything was sent",
        }
        .render();
        assert!(rendered.contains("DNS lookup failed before anything was sent"));
        assert!(rendered.contains("Do not repeat completed actions"));
        assert!(rendered.contains("Keep the same turn_token and result schema"));
        assert!(!rendered.contains("{failure}"));
    }
    use super::*;

    #[test]
    fn child_assignment_prompts_are_stable() {
        assert!(PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_DIRECT.starts_with("Template:"));
        assert!(
            PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_DIRECT
                .contains("retry only if the tool result says the draft was rejected")
        );
        for template in [
            PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_DIRECT,
            PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_TOOLS_PTC,
            PLAK_CHILD_ASSIGNMENT_START_TEMPLATE_PY_PTC,
        ] {
            assert!(template.contains("Tier: {tier}"));
            assert!(template.contains("Native plan or todo updates are not submission."));
            assert!(template.ends_with("{tier_line}"));
            assert!(!template.contains("write_effect"));
            assert!(!template.contains("merge_workspace"));
            assert!(!template.contains("workspace_mode"));
            assert!(!template.contains("{class}"));
        }
        assert_eq!(
            PLAK_CHILD_ASSIGNMENT_WORKSPACE_LINE_TEMPLATE,
            "Workspace: {path}\n"
        );
        assert_eq!(PLAK_CHILD_ASSIGNMENT_MESSAGE_PREFIX, "\n\nMessage:\n");
    }

    /// the Start prompt names the workspace the child actually
    /// has, and exactly one tier line is rendered.
    #[test]
    fn each_tier_renders_one_workspace_line() {
        for (tier, isolated, expected, needle) in [
            (
                AgentTier::ReadOnly,
                false,
                PLAK_CHILD_TIER_LINE_READ_ONLY,
                "live checkout",
            ),
            (
                AgentTier::ReadOnly,
                true,
                PLAK_CHILD_TIER_LINE_READ_ONLY_ISOLATED,
                "own snapshot copy",
            ),
            (
                AgentTier::Execute,
                false,
                PLAK_CHILD_TIER_LINE_EXECUTE,
                "share the parent's live checkout",
            ),
            (
                AgentTier::Execute,
                true,
                PLAK_CHILD_TIER_LINE_EXECUTE_ISOLATED,
                "own snapshot copy",
            ),
            (
                AgentTier::Mutate,
                false,
                PLAK_CHILD_TIER_LINE_MUTATE,
                "share the parent's live checkout",
            ),
            (
                AgentTier::Mutate,
                true,
                PLAK_CHILD_TIER_LINE_MUTATE_ISOLATED,
                "own fork",
            ),
        ] {
            let line = child_tier_line(tier, isolated);
            assert_eq!(line, expected);
            assert!(line.contains(needle), "{line}");
            assert_eq!(line.matches("Your tier is").count(), 1);
        }
        for tier in [AgentTier::ReadOnly, AgentTier::Execute] {
            for isolated in [false, true] {
                let line = child_tier_line(tier, isolated);
                assert!(line.contains("File-writing tools are not presented"));
                assert!(line.contains("tools stay allowed when presented"));
            }
        }
        assert!(PLAK_CHILD_TIER_LINE_READ_ONLY.contains("adopt_candidate and discard_workspace"));
    }

    #[test]
    fn child_submit_nudges_are_stable() {
        assert!(PLAK_CHILD_SUBMIT_NUDGE_REMINDER_TEMPLATE.starts_with("Your previous"));
        assert!(PLAK_CHILD_SUBMIT_NUDGE_FORCE_TEMPLATE.starts_with("Required:"));
    }

    /// The typed renderer must write exactly what the retired `replace` chain
    /// at the assembly site wrote, for every tier and both optional parts.
    #[test]
    fn typed_start_renders_the_retired_substitution_chain() {
        for tier in [AgentTier::ReadOnly, AgentTier::Execute, AgentTier::Mutate] {
            for kind in [
                ChildAssignmentStartKind::Direct,
                ChildAssignmentStartKind::ToolsPtc,
                ChildAssignmentStartKind::PyPtc,
            ] {
                for workspace_path in [None, Some("/repo/.plak/worktrees/c1")] {
                    for message in [None, Some("hello")] {
                        let start = ChildAssignmentStart {
                            kind,
                            template: "reviewer",
                            tier,
                            description: "",
                            prompt: "check it",
                            schema: "{\"type\":\"object\"}",
                            turn_token: 7,
                            workspace_path,
                            message,
                        };
                        let rendered = start.render();
                        for placeholder in [
                            "{template}",
                            "{tier}",
                            "{workspace_line}",
                            "{tier_line}",
                            "{description}",
                            "{prompt}",
                            "{schema}",
                        ] {
                            assert!(
                                !rendered.contains(placeholder),
                                "{placeholder} in {rendered}"
                            );
                        }
                        assert!(rendered.contains("reviewer"));
                        assert!(rendered.contains("check it"));
                        assert!(
                            rendered.ends_with(child_tier_line(tier, workspace_path.is_some()))
                                || message.is_some()
                        );
                        assert_eq!(
                            rendered.contains("Workspace: /repo/.plak/worktrees/c1"),
                            workspace_path.is_some()
                        );
                        assert_eq!(rendered.contains("\n\nMessage:\nhello"), message.is_some());
                    }
                }
            }
        }
    }

    #[test]
    fn submit_nudge_escalates_once_and_renders_both_fields() {
        assert_eq!(SubmitNudge::after(0), SubmitNudge::Reminder);
        assert_eq!(SubmitNudge::after(1), SubmitNudge::Reminder);
        assert_eq!(SubmitNudge::after(2), SubmitNudge::Force);
        for nudge in [SubmitNudge::Reminder, SubmitNudge::Force] {
            let rendered = nudge.render(9, "{}");
            assert!(rendered.contains("turn_token=9"), "{rendered}");
            assert!(!rendered.contains("{turn_token}"));
            assert!(!rendered.contains("{schema}"));
        }
    }
}

/// A new instruction turn on the same assignment after a committed transient failure.
#[derive(Clone, Copy, Debug)]
pub struct TransientResume<'a> {
    /// Sanitized canonical failure text, not a reconstructed provider request.
    pub failure: &'a str,
}

impl TransientResume<'_> {
    /// Continues completed work without replaying actions or changing the result token.
    #[must_use]
    pub fn render(self) -> String {
        PLAK_CHILD_TRANSIENT_RESUME_TEMPLATE.replace("{failure}", self.failure)
    }
}

/// Transient continuation instruction; `{failure}` is sanitized canonical text.
pub const PLAK_CHILD_TRANSIENT_RESUME_TEMPLATE: &str = "The previous turn failed: {failure}\n\nContinue the same assignment from the last completed step. Do not repeat completed actions. This is a new turn, not a replay of the failed request. Keep the same turn_token and result schema; call submit_result when the assignment is done.";
