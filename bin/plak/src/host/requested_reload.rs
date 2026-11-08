//! The service owns hosted-session reload announcements and refusals, even without a client.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptRow};
use plakous_hosted::host_service::wire::{SessionHost, SlotView};

use crate::product_paths::PlakPaths;
use crate::reload_request::{Listener, ReloadOutcome};
use crate::surface_protocol::{HostEvent, PlakExtension};

pub(super) async fn serve(host: Arc<SessionHost<PlakExtension>>, paths: PlakPaths) {
    let mut listeners = BTreeMap::<String, Listener>::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tick.tick().await;
        tokio::task::block_in_place(|| poll(&host, &paths, &mut listeners));
    }
}

fn poll(
    host: &SessionHost<PlakExtension>,
    paths: &PlakPaths,
    listeners: &mut BTreeMap<String, Listener>,
) {
    let mut resident = BTreeSet::new();
    for slot in host.slots() {
        let SlotView::Resident(session) = slot.peek() else {
            continue;
        };
        if session.ended() {
            continue;
        }
        let id = session.session_id();
        resident.insert(id.clone());
        if listeners
            .get(&id)
            .is_none_or(|listener| !listener.is_current())
            && let Some(listener) = Listener::announce_hosted(paths, &id)
        {
            listeners.insert(id.clone(), listener);
        }
        let Some(request) = listeners
            .get(&id)
            .and_then(|listener| listener.take(paths, &id))
        else {
            continue;
        };
        let welcome = host.welcome();
        let build = if welcome.binary_id.is_empty() || welcome.binary_id == "unknown" {
            String::new()
        } else {
            format!(" (build {})", welcome.binary_id)
        };
        let reason = format!(
            "this session runs in plak host{build}; restarting the host moves it onto the installed build (plak host stop, or restart its unit)"
        );
        crate::reload_request::record(
            paths,
            &request.request_id,
            &ReloadOutcome::Refused {
                reason: reason.clone(),
            },
        );
        session
            .publisher()
            .publish(HostEvent::Row(TranscriptRow::warning(
                NoticeScope::Settings,
                format!("requested reload refused: {reason}"),
            )));
    }
    listeners.retain(|id, _| resident.contains(id));
}
