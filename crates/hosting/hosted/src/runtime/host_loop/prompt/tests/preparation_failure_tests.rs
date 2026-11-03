use super::*;
use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
use crate::surface_protocol::UserAction;
use plakous_host_protocol::status::{HostPhase, HostStatusDetail};

struct PauseWriteFailure;

impl StoreFaults for PauseWriteFailure {
    fn before(&self, call: StoreCall, _: Option<&SessionRecord>) -> Result<(), String> {
        if call == StoreCall::WriteHostControls {
            Err("injected pause write failure".into())
        } else {
            Ok(())
        }
    }
}

#[test]
fn stopped_preparation_publishes_stop_status_even_when_the_pause_write_fails() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (wire, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        NodeActor::open_blocking(config, &receiver, crate::runtime::Sender::over(wire), None)
            .unwrap();
    let item = host.config.session.enqueue_live("keep queued").unwrap();
    host.config.store.save(&mut host.config.session).unwrap();
    host.config.store = Arc::new(FaultStore::new(
        host.config.store.clone(),
        PauseWriteFailure,
    ));
    events.try_iter().for_each(drop);
    host.prep_hooks.hook = Some((
        preparation::Stage::Settle,
        Box::pin(async move {
            actions.send(UserAction::Interrupt).unwrap();
        }),
    ));
    let error = host
        .block(async |host| {
            host.run_prompt(item.text.clone(), Some(item), true, None)
                .await
        })
        .err()
        .expect("pause persistence must fail");
    assert!(error.message().contains("injected pause write failure"));
    let statuses: Vec<_> = events
        .try_iter()
        .filter_map(|event| {
            if let HostEvent::Status { phase, detail, .. } = event {
                Some((phase, detail))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        statuses.last(),
        Some(&(
            HostPhase::Idle,
            Some(HostStatusDetail::PromptPreparationStopped)
        ))
    );
    assert!(
        !statuses
            .iter()
            .any(|(_, detail)| matches!(detail, Some(HostStatusDetail::PromptNotAdmitted)))
    );
    host.block(async |host| host.release().await).unwrap();
}
