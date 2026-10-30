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
                if tool == "write_stdin" && host_wakes {
                    assert!(!text.contains("sleep-poll"));
                } else {
                    assert!(text.contains("sleep-poll") || text.contains("do not poll"));
                }
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
        hub.find(PLAK_PRODUCT_TOPOLOGY_AGREEMENTS) < hub.find(PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS)
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
            "plak.agreements.process-sdk-wait",
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
            "plak.agreements.process-sdk-wake",
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
