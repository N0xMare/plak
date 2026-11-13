//! Explicit test-only reservation hold; no service lock is held while waiting.

use super::HostService;
use plakous_host_protocol::wire::{ClientDraftId, RequestRefusal};
use plakous_host_protocol::{IntentSender, ProtocolExtension, SessionIntent};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

struct Gate {
    id: Option<ClientDraftId>,
    claimed: AtomicBool,
    wait: Arc<dyn Fn(ClientDraftId) -> bool + Send + Sync>,
}

static GATE: OnceLock<Gate> = OnceLock::new();

/// Hold the first real reservation of `id`, or any ID when `id` is `None`.
/// `wait` runs outside the host loop and all service locks. True releases the
/// unchanged intent; false drops the pending reservation without admission.
/// Its argument is the original reserved ID, never a fixture replacement.
/// This port is absent unless the repository's test-support feature is enabled.
///
/// # Errors
/// A fixture already installed a gate in this process.
pub fn install(
    id: Option<ClientDraftId>,
    wait: Arc<dyn Fn(ClientDraftId) -> bool + Send + Sync>,
) -> Result<(), &'static str> {
    GATE.set(Gate {
        id,
        claimed: AtomicBool::new(false),
        wait,
    })
    .map_err(|_| "draft gate already installed")
}

pub(super) fn defer<P: ProtocolExtension>(
    service: &HostService<P>,
    sender: &IntentSender<P>,
    intent: &SessionIntent<P>,
    client: Option<&str>,
    pending: &std::collections::BTreeMap<ClientDraftId, super::drafts::PendingDraft>,
) -> Result<bool, RequestRefusal> {
    let Some(gate) = GATE.get() else {
        return Ok(false);
    };
    let Some(id) = client.and_then(|id| id.parse::<ClientDraftId>().ok()) else {
        return Ok(false);
    };
    if gate.id.is_some_and(|selected| selected != id)
        || !pending.contains_key(&id)
        || !matches!(
            intent,
            SessionIntent::Prompt(_)
                | SessionIntent::FollowUp(_)
                | SessionIntent::Steer(_)
                | SessionIntent::MailTopologyNode { .. }
        )
        || gate.claimed.swap(true, Ordering::AcqRel)
    {
        return Ok(false);
    }
    let service = service.clone();
    let sender = sender.clone();
    let intent = intent.clone();
    let client = client.map(str::to_owned);
    std::thread::Builder::new()
        .name("fixture-draft-hold".into())
        .spawn(move || {
            if !(gate.wait)(id) || sender.send_correlated(intent, client).is_err() {
                service.refuse_wire_draft(id);
            }
        })
        .map_err(|_| RequestRefusal::Disconnected)?;
    Ok(true)
}
