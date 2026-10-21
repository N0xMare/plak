//! Child mail wakes the real process observer, without steering or killing it.
use super::recover_tests::{script, scripted_host_with, turn_completed, wait_for};
use super::test_support::{install_live_descendant_on_hub, send_attached_action};
use crate::agent_tree_protocol::{MessagePurpose, SendAgentMessageRequest};
use crate::runtime::spawn_host_worker;
use crate::runtime::test_kit::TestExtension;

type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

#[test]
fn child_mail_preempts_a_waiting_process_in_a_root_turn() {
    let root = tempfile::tempdir().unwrap();
    let mut plan = script(serde_json::json!([
        {"output":{"tool":{"name":"process_session_start","arguments":{"command":"sleep 60","workdir":".","login":false,"tty":false,"keep_stdin_open":false,"yield_time_ms":600_000,"max_output_bytes":4096}}},"finish":"tool_calls"},
        {"output":{"text":"mail can now be inspected"}}
    ]));
    std::sync::Arc::get_mut(&mut plan).unwrap().process_tools = true;
    let (config, _) = scripted_host_with(root.path(), plan);
    let id = config.session.id.clone();
    let jobs = config.state_roots.job_registry().clone();
    install_live_descendant_on_hub(&id, &config.cwd);
    let hub = crate::topology_runtime::lookup(&id).unwrap();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("run and observe".into()));
    wait_for(
        &events,
        &mut seen,
        "the waiting process",
        |event| matches!(event, HostEvent::ToolLine { name, state, .. } if name == "process_session_start" && state == "running"),
    );
    let message_id = hub
        .mutate_then_emit(false, |tree| {
            let child = tree
                .nodes()
                .keys()
                .find(|id| *id != &tree.root_agent_id)
                .unwrap()
                .clone();
            tree.send_agent_message(
                &child,
                &SendAgentMessageRequest {
                    agent_id: tree.root_agent_id.clone(),
                    purpose: Some(MessagePurpose::Question),
                    message: "new information while the process is quiet".into(),
                    output_schema_json: None,
                    priority: None,
                    in_reply_to: None,
                },
                None,
            )
            .map(|receipt| receipt.message_id)
            .map_err(|error| error.message)
        })
        .unwrap();
    wait_for(
        &events,
        &mut seen,
        "the root completion before its 600s hint",
        turn_completed,
    );
    assert!(
        jobs.views()
            .iter()
            .any(|job| job.status == plakous::tools::JobStatus::Running),
        "notice must return a running partial, not kill the process"
    );
    assert!(
        hub.read(|tree| tree
            .pending_mail(&tree.root_agent_id)
            .iter()
            .any(|mail| mail.message_id == message_id))
            .unwrap(),
        "yielding must not acknowledge mail"
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

fn process_host(
    root: &std::path::Path,
    steps: serde_json::Value,
) -> crate::runtime::HostConfig<TestExtension> {
    use plakous_offline_crash_harness::{
        BoundaryController, protocol::CrashBoundary, scripted_runtime_binding_with_extensions,
    };
    use std::sync::{Arc, Mutex, atomic::AtomicU64};
    let plan = script(steps);
    let (mut config, _) = scripted_host_with(root, Arc::clone(&plan));
    let workspace = config.cwd.clone();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.join("notice-evidence.jsonl"),
    )
    .unwrap();
    let cursor = Arc::new(AtomicU64::new(1));
    let completion_notices = config.prompt_host_facts().host_wakes;
    let build = Arc::new(move || {
        let tools = Arc::new(
            plakous::tools::ProcessSessionTools::posix_sh()
                .map_err(|e| e.to_string())?
                .with_config(plakous::tools::ProcessPackConfig { completion_notices }),
        );
        let mut registrations = tools.registrations().map_err(|e| e.to_string())?;
        registrations.sort_by(|left, right| left.name().cmp(right.name()));
        scripted_runtime_binding_with_extensions(
            &workspace,
            controller.clone(),
            Arc::clone(&plan),
            Arc::clone(&cursor),
            registrations,
        )
        .map_err(|e| e.to_string())
    });
    let target = build().unwrap().target_binding();
    config.test_injection = Some(Arc::new(crate::runtime::TestHostInjection {
        runtime: Arc::new(move |_| build()),
        rebind_target: target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: Mutex::new(Vec::new()),
    }));
    config
}

fn start_call(command: &str, hint: u64, open: bool) -> serde_json::Value {
    serde_json::json!({"name":"process_session_start", "arguments":{
        "command":command,"workdir":".","login":false,"tty":false,"keep_stdin_open":open,
        "yield_time_ms":hint,"max_output_bytes":4096
    }})
}

fn poll_call(input: &str) -> serde_json::Value {
    serde_json::json!({"name":"process_session_write","arguments":{
        "session_id":1,"input":input,"disposition":"keep_open","yield_time_ms":5000,"max_output_bytes":4096
    }})
}

fn send_question(hub: &crate::topology_runtime::ProductTopologyRuntime) {
    hub.mutate_then_emit(false, |tree| {
        let child = tree
            .nodes()
            .keys()
            .find(|id| *id != &tree.root_agent_id)
            .unwrap()
            .clone();
        tree.send_agent_message(
            &child,
            &SendAgentMessageRequest {
                agent_id: tree.root_agent_id.clone(),
                purpose: Some(MessagePurpose::Question),
                message: "still unread until the root consumes it".into(),
                output_schema_json: None,
                priority: None,
                in_reply_to: None,
            },
            None,
        )
        .map(|_| ())
        .map_err(|error| error.message)
    })
    .unwrap();
}

#[test]
fn unread_mail_preempts_each_following_repeat_poll() {
    let root = tempfile::tempdir().unwrap();
    let config = process_host(
        root.path(),
        serde_json::json!([
            {"output":{"tool": start_call("read line", 250, true)},"finish":"tool_calls"},
            {"output":{"tool": poll_call("")},"finish":"tool_calls"},
            {"output":{"tool": poll_call("")},"finish":"tool_calls"},
            {"output":{"tool": poll_call("")},"finish":"tool_calls"},
            {"output":{"text":"all three polls yielded"}}
        ]),
    );
    let id = config.session.id.clone();
    install_live_descendant_on_hub(&id, &config.cwd);
    let hub = crate::topology_runtime::lookup(&id).unwrap();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("observe the quiet job".into()));
    let polls = std::cell::RefCell::new(std::collections::HashSet::new());
    wait_for(&events, &mut seen, "the second, held poll", |event| {
        if let HostEvent::ToolLine {
            name,
            state,
            call_id,
            ..
        } = event
            && name == "process_session_write"
            && state == "running"
        {
            polls.borrow_mut().insert(call_id.clone());
        }
        polls.borrow().len() == 2
    });
    send_question(&hub);
    wait_for(
        &events,
        &mut seen,
        "the later poll yielding while mail stays unread",
        turn_completed,
    );
    assert!(
        format!("{seen:?}").contains("New messages are waiting; end this turn to receive them."),
        "mail yield omitted its model-visible reason"
    );
    assert!(
        !hub.read(|tree| tree.pending_mail(&tree.root_agent_id).is_empty())
            .unwrap()
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

#[test]
fn a_job_joined_in_band_does_not_preempt_the_next_foreground_start() {
    let root = tempfile::tempdir().unwrap();
    let config = process_host(
        root.path(),
        serde_json::json!([
            {"output":{"tool": start_call("read line", 250, true)},"finish":"tool_calls"},
            {"output":{"tools": [poll_call("finish\n"), start_call("sleep 1; printf next", 5000, false)]},"finish":"tool_calls"},
            {"output":{"text":"the next command completed inline"}}
        ]),
    );
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(
        &actions,
        UserAction::Prompt("join then run another command".into()),
    );
    wait_for(
        &events,
        &mut seen,
        "the completed foreground start",
        turn_completed,
    );
    let promoted = seen
        .iter()
        .filter_map(|event| {
            if let HostEvent::JobPromoted { job_id, .. } = event {
                Some(job_id.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        promoted,
        ["p1"],
        "an in-band terminal observation leaked into the next dispatch"
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

#[test]
fn an_unconsumed_job_terminal_preempts_foreground_once_per_turn() {
    let root = tempfile::tempdir().unwrap();
    let config = process_host(
        root.path(),
        serde_json::json!([
            {"output":{"tool": start_call("while [ ! -f finish ]; do sleep 0.01; done", 250, false)},"finish":"tool_calls"},
            {"output":{"tools": [start_call("read line", 600_000, true), start_call("sleep 1; printf next", 5000, false)]},"finish":"tool_calls"},
            {"output":{"text":"the terminal notice woke only the first wait"}}
        ]),
    );
    let finish = config.cwd.join("finish");
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(
        &actions,
        UserAction::Prompt("run two foreground observations".into()),
    );
    let calls = std::cell::RefCell::new(std::collections::HashSet::new());
    wait_for(
        &events,
        &mut seen,
        "the second foreground observation",
        |event| {
            if let HostEvent::ToolLine {
                name,
                state,
                call_id,
                ..
            } = event
                && name == "process_session_start"
                && state == "running"
            {
                calls.borrow_mut().insert(call_id.clone());
            }
            calls.borrow().len() == 2
        },
    );
    std::fs::write(finish, "finish").unwrap();
    wait_for(
        &events,
        &mut seen,
        "the driver-owned terminal wake and inline next start",
        turn_completed,
    );
    let promoted = seen
        .iter()
        .filter_map(|event| {
            if let HostEvent::JobPromoted { job_id, .. } = event {
                Some(job_id.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        promoted,
        ["p1", "p2"],
        "one terminal must not preempt a later dispatch again"
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

#[test]
fn a_notices_off_root_does_not_preempt_on_a_job_terminal() {
    let root = tempfile::tempdir().unwrap();
    // Compile a notices-capable pack, then revoke host delivery for this turn.
    let mut config = process_host(
        root.path(),
        serde_json::json!([
            {"output":{"tool": start_call("while [ ! -f finish ]; do sleep 0.01; done", 250, false)},"finish":"tool_calls"},
            {"output":{"tool": start_call("while [ ! -f release ]; do sleep 0.01; done", 600_000, false)},"finish":"tool_calls"},
            {"output":{"text":"the foreground command completed inline"}}
        ]),
    );
    config.auto_continue = false;
    assert!(!config.prompt_host_facts().host_wakes);
    let finish = config.cwd.join("finish");
    let release = config.cwd.join("release");
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(
        &actions,
        UserAction::Prompt("observe without host wakes".into()),
    );
    let calls = std::cell::RefCell::new(std::collections::HashSet::new());
    wait_for(&events, &mut seen, "the foreground observation", |event| {
        if let HostEvent::ToolLine {
            name,
            state,
            call_id,
            ..
        } = event
            && name == "process_session_start"
            && state == "running"
        {
            calls.borrow_mut().insert(call_id.clone());
        }
        calls.borrow().len() == 2
    });
    std::fs::write(finish, "finish").unwrap();
    wait_for(
        &events,
        &mut seen,
        "the background terminal",
        |event| matches!(event, HostEvent::JobTerminal { job_id, .. } if job_id == "p1"),
    );
    std::fs::write(release, "release").unwrap();
    wait_for(
        &events,
        &mut seen,
        "the inline foreground completion",
        turn_completed,
    );
    let promoted = seen
        .iter()
        .filter_map(|event| {
            if let HostEvent::JobPromoted { job_id, .. } = event {
                Some(job_id.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(promoted, ["p1"]);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

#[test]
fn unread_mail_yields_are_bounded_within_one_tool_batch() {
    let root = tempfile::tempdir().unwrap();
    let config = process_host(
        root.path(),
        serde_json::json!([
            {"output":{"tool": start_call("read line", 250, true)},"finish":"tool_calls"},
            {"output":{"tools": [poll_call(""), poll_call(""), poll_call(""), poll_call(""), poll_call("")]},"finish":"tool_calls"},
            {"output":{"text":"polling cannot create an unbounded fast-yield loop"}}
        ]),
    );
    let id = config.session.id.clone();
    install_live_descendant_on_hub(&id, &config.cwd);
    let hub = crate::topology_runtime::lookup(&id).unwrap();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(
        &actions,
        UserAction::Prompt("poll repeatedly in one batch".into()),
    );
    let polls = std::cell::RefCell::new(std::collections::HashSet::new());
    wait_for(&events, &mut seen, "the held repeat poll", |event| {
        if let HostEvent::ToolLine {
            name,
            state,
            call_id,
            ..
        } = event
            && name == "process_session_write"
            && state == "running"
        {
            polls.borrow_mut().insert(call_id.clone());
        }
        polls.borrow().len() == 2
    });
    send_question(&hub);
    let began = std::time::Instant::now();
    wait_for(&events, &mut seen, "bounded later polls", turn_completed);
    assert!(
        began.elapsed() >= std::time::Duration::from_secs(10),
        "mail repeatedly forced short yields within one batch"
    );
    assert!(
        format!("{seen:?}").contains("New messages are waiting; end this turn to receive them.")
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}
