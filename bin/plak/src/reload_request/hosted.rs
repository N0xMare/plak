use super::{
    Listener, PlakPaths, enabled, fs, listener_path, private_directory, read_announcement,
};

impl Listener {
    /// A hosted session leaves a live announcer in place. The service, not
    /// any client, owns its announcement and instance-bound request polling.
    pub(crate) fn announce_hosted(paths: &PlakPaths, session: &str) -> Option<Self> {
        use std::os::unix::fs::OpenOptionsExt as _;
        if !enabled(paths) {
            return None;
        }
        // Serialize the live-pid check with publication so a concurrent
        // owner never erases a request for a just-published peer.
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(private_directory(paths).ok()?.join("announcements.lock"))
            .ok()?;
        lock.try_lock().ok()?;
        if read_announcement(&listener_path(paths, session)).is_some_and(|announcement| {
            rustix::process::Pid::from_raw(announcement.pid)
                .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
        }) {
            return None;
        }
        Self::announce(paths, session)
    }
}
