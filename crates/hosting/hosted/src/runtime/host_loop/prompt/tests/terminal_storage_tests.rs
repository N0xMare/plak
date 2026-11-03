//! Queue identity through a refused terminal projection and in-place recovery.

use super::*;
use crate::runtime::fault_store::{StoreCall, StoreFaults};
use crate::surface_protocol::UserAction;
use plakous_offline_crash_harness::gate::GateHandle;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct TerminalCapacityFault {
    write: CapacityWriteFault,
    gate: Arc<GateHandle>,
    refused: Arc<AtomicUsize>,
    queued: Arc<Mutex<Option<queue::QueueItem>>>,
}

impl StoreFaults for TerminalCapacityFault {
    fn refusal(&self, call: StoreCall) -> Option<crate::error::StoreRefusal> {
        let refusal = self.write.refusal(call);
        if refusal.is_some() {
            self.refused.fetch_add(1, Ordering::SeqCst);
        }
        refusal
    }

    fn before(&self, call: StoreCall, session: Option<&SessionRecord>) -> Result<(), String> {
        self.write.before(call, session)?;
        if call == StoreCall::Save
            && let Some(item) = session.and_then(|record| {
                record
                    .pending_queue
                    .iter()
                    .find(|item| item.text == "committed follow-up")
            })
        {
            self.queued
                .lock()
                .unwrap()
                .get_or_insert_with(|| item.clone());
            self.gate.release();
        }
        Ok(())
    }
}

#[test]
fn enospc_through_definite_terminal_publication_recovers_without_merging_queue() {
    assert_terminal_storage_full_recovers_without_merging(false);
}

#[test]
fn enospc_through_unknown_terminal_publication_recovers_without_merging_queue() {
    assert_terminal_storage_full_recovers_without_merging(true);
}

fn assert_terminal_storage_full_recovers_without_merging(unknown: bool) {
    let root = tempfile::tempdir().unwrap();
    let gate = Arc::new(GateHandle::new());
    let kind = if unknown {
        "outcome_unknown"
    } else {
        "protocol"
    };
    let plan = crate::runtime::recover_tests::script(serde_json::json!([
        {"gate": gate.script(), "fault": {"at": "in_band", "kind": kind, "retryability": "Permanent"}},
        {"output": {"text": "next response"}},
        {"output": {"text": "queued response"}}
    ]));
    let (mut config, _) = crate::runtime::recover_tests::scripted_host_with(root.path(), plan);
    let durable = config.store.clone();
    let id = config.session.id.clone();
    let full = Arc::new(AtomicBool::new(false));
    let refused = Arc::new(AtomicUsize::new(0));
    let captured = Arc::new(Mutex::new(None));
    config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
        durable.clone(),
        TerminalCapacityFault {
            write: CapacityWriteFault(full.clone()),
            gate,
            refused: refused.clone(),
            queued: captured.clone(),
        },
    ));
    let arm = full.clone();
    let once = AtomicBool::new(true);
    crate::runtime::test_kit::scripted_mut(&mut config).observe =
        Arc::new(move |workflow, boundary| {
            if workflow == HostWorkflow::RootDelivery
                && boundary == HostBoundary::TerminalReady
                && once.swap(false, Ordering::SeqCst)
            {
                arm.store(true, Ordering::SeqCst);
            }
        });
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (sender, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(
        config,
        &receiver,
        crate::runtime::Sender::over(sender),
        None,
    )
    .unwrap();
    actions
        .send(UserAction::Prompt("root prompt".into()))
        .unwrap();
    let step = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();
    actions
        .send(UserAction::FollowUp("committed follow-up".into()))
        .unwrap();
    run_prompt_step(&mut host, step);
    assert!(
        full.load(Ordering::SeqCst),
        "outage spans terminal publication"
    );
    assert!(
        host.role.recovery.blocked(),
        "the refused projection fences admission"
    );
    assert!(refused.load(Ordering::SeqCst) > 0);
    let queued = captured
        .lock()
        .unwrap()
        .clone()
        .expect("follow-up saved while the root was busy");
    assert_eq!(queued.text, "committed follow-up");
    let mut seen: Vec<_> = events.try_iter().collect();
    let note = if unknown {
        "recovery-required terminal projection could not be committed"
    } else {
        "definite turn failure could not be saved"
    };
    assert!(seen.iter().any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(row)) if row.text.contains(note))), "{seen:#?}");
    assert_retained_queue(&durable.load(&id).unwrap(), &seen, &queued);

    full.store(false, Ordering::SeqCst);
    actions
        .send(UserAction::Prompt("distinct next prompt".into()))
        .unwrap();
    let step = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();
    assert!(
        !host.role.recovery.blocked(),
        "the new prompt recovered in place"
    );
    run_prompt_step(&mut host, step);
    seen.extend(events.try_iter());
    assert!(
        seen.iter()
            .any(|event| matches!(event, HostEvent::Recovered { .. }))
    );
    assert_retained_queue(&durable.load(&id).unwrap(), &seen, &queued);
    assert_retained_queue(&host.role.config.session, &seen, &queued);
    actions.send(UserAction::ResumeQueueDrain).unwrap();
    let step = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();
    run_prompt_step(&mut host, step);
    seen.extend(events.try_iter());
    assert_busy_follow_up_delivered(&seen, true);
    assert_eq!(
        durable.load(&id).unwrap().pending_queue,
        [] as [queue::QueueItem; 0]
    );
    host.block(async |host| host.release().await).unwrap();
}

fn run_prompt_step(host: &mut RootActor<'_, TestExtension>, step: HostStep) {
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = step
    else {
        panic!("expected a prompt");
    };
    host.block(async |host| {
        host.run_prompt(text, queued_delivery, user_echoed, None)
            .await
    })
    .unwrap();
}

fn assert_retained_queue(
    record: &SessionRecord,
    seen: &[HostEvent<TestExtension>],
    queued: &queue::QueueItem,
) {
    assert_eq!(record.pending_queue, std::slice::from_ref(queued));
    let items = seen
        .iter()
        .rev()
        .find_map(|event| match event {
            HostEvent::QueueSnapshot { items, .. } => Some(items),
            _ => None,
        })
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        (&items[0].id, items[0].revision, &items[0].text),
        (&queued.id, queued.revision, &queued.text)
    );
}
