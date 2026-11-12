//! Preserve the selected fixture's real opening observations until its first attachment.

use super::{HostService, ProtocolExtension};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static SESSION: OnceLock<String> = OnceLock::new();

/// Hold the selected session's initial observation pump until its first attachment.
/// The runtime opens normally; no observation is invented, rewritten or replayed.
///
/// # Errors
/// The selection is not a UUID, or this process already selected a fixture.
pub fn install(session: String) -> Result<(), &'static str> {
    uuid::Uuid::parse_str(&session).map_err(|_| "recovery fixture session must be a UUID")?;
    SESSION
        .set(session)
        .map_err(|_| "recovery fixture already selected")
}

pub(super) fn before_pump<P: ProtocolExtension>(
    session: &str,
    service: &HostService<P>,
) -> impl FnOnce() -> bool + Send + 'static {
    let held = SESSION
        .get()
        .filter(|selected| *selected == session)
        .map(|_| service.clone());
    move || {
        let Some(service) = held else { return true };
        let until = Instant::now() + Duration::from_secs(120);
        while Instant::now() < until {
            if service
                .lock()
                .attachments
                .values()
                .any(|held| !held.dropped)
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }
}
