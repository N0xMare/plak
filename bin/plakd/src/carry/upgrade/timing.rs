use std::time::Duration;

pub(crate) const PROBE: Duration = Duration::from_secs(30);
pub(crate) const ADMISSION: Duration = Duration::from_secs(15);
pub(crate) const STORE: Duration = Duration::from_secs(90);
pub(crate) const BINDINGS: Duration = Duration::from_secs(120);
pub(crate) const CAPTURE: Duration = Duration::from_secs(120);
pub(crate) const BACKUP: Duration = Duration::from_secs(60);
pub(crate) const REVISION_BACKUP: Duration = Duration::from_secs(60);

// Discovery, runner inspection, runtime rehearsal, and checkpoint staging
// share this reserve. No request may start once the preflight bound expires.
pub(crate) const PREFLIGHT: Duration = Duration::from_secs(
    STORE.as_secs() + BINDINGS.as_secs() + CAPTURE.as_secs() + BACKUP.as_secs() + 300,
);
// Qualification and mandatory joins, plus the capture under the source holds.
pub(crate) const SOURCE_REPLY: Duration = Duration::from_secs(600 + CAPTURE.as_secs());
// Includes stopped revisions other than the validated backup and startup.
pub(crate) const SUCCESSOR: Duration = Duration::from_secs(
    ADMISSION.as_secs()
        + STORE.as_secs()
        + CAPTURE.as_secs()
        + REVISION_BACKUP.as_secs()
        + BINDINGS.as_secs()
        + 300,
);

pub(crate) fn deploy_request(drain: u32) -> Duration {
    PREFLIGHT + SOURCE_REPLY + SUCCESSOR + Duration::from_secs(u64::from(drain)) + PROBE * 3
}
