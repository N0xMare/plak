use super::super::{SELECTOR, hook_environment};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) fn install_pending_gate() -> Result<(), String> {
    if let Some(session) = hook_environment("PLAK_TEST_RECOVERY_FIRST_ATTACH") {
        if hook_environment("PLAK_TEST_MODEL").as_deref() != Some(std::ffi::OsStr::new(SELECTOR)) {
            return Err("recovery fixture requires the offline test model".into());
        }
        plakous_hosted::host_service::test_open_observations::install(
            session
                .into_string()
                .map_err(|_| "invalid recovery fixture session")?,
        )
        .map_err(str::to_owned)?;
    }
    let values = [
        "PLAK_TEST_DRAFT_PENDING_ID",
        "PLAK_TEST_DRAFT_PENDING_READY",
        "PLAK_TEST_DRAFT_PENDING_RELEASE",
    ]
    .map(hook_environment);
    if values.iter().all(Option::is_none) {
        return Ok(());
    }
    if hook_environment("PLAK_TEST_MODEL").as_deref() != Some(std::ffi::OsStr::new(SELECTOR)) {
        return Err("draft gate requires the offline test model".into());
    }
    let [Some(id), Some(ready), Some(release)] = values else {
        return Err("draft gate requires ID, READY and RELEASE".into());
    };
    let id = match id.to_str().ok_or("invalid draft gate ID")? {
        "first" => None,
        value => Some(value.parse().map_err(|_| "invalid draft gate ID")?),
    };
    let (ready, release) = (PathBuf::from(ready), PathBuf::from(release));
    if !ready.is_absolute()
        || !release.is_absolute()
        || ready == release
        || ready.exists()
        || release.exists()
    {
        return Err("draft gate needs distinct new absolute barrier files".into());
    }
    plakous_hosted::host_service::test_draft_gate::install(
        id,
        Arc::new(move |reserved_id| {
            use std::io::Write;
            let marker = match id {
                Some(_) => "reserved\n".to_owned(),
                None => format!("reserved {reserved_id}\n"),
            };
            let written = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&ready)
                .and_then(|mut file| {
                    file.write_all(marker.as_bytes())?;
                    file.sync_all()
                });
            if written.is_err() {
                return false;
            }
            let until = Instant::now() + Duration::from_secs(120);
            while Instant::now() < until && crate::signals::signalled_stops() == 0 {
                match std::fs::read(&release) {
                    Ok(value) => return value == b"release\n" || value == b"release",
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return false,
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        }),
    )
    .map_err(str::to_owned)
}
