#[test]
fn child_prompt_pins_match_unchanged_text() {
    assert_eq!(
        plakous::tools::drifted_pins(CHILD_PROMPT_PINS),
        [] as [(&str, String); 0]
    );
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
