use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::product_config::ProductConfig;
use crate::product_config::metadata::{FileStamp, file_stamp};

use super::{
    Listener, PlakPaths, ReloadRequest, fs, listener_path, private_directory, read_announcement,
};

pub(crate) struct HostedListener {
    listener: Listener,
    stamp: Option<FileStamp>,
}

impl HostedListener {
    /// A hosted session leaves a live announcer in place. The service, not
    /// any client, owns its announcement and instance-bound request polling.
    pub(crate) fn announce(paths: &PlakPaths, session: &str) -> Option<Self> {
        use std::os::unix::fs::OpenOptionsExt as _;
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
        let listener = Listener::announce_enabled(paths, session)?;
        Some(Self {
            listener,
            stamp: None,
        })
    }

    pub(crate) fn is_current(&mut self) -> bool {
        // Sample before checking the nonce so a racing replacement cannot
        // acquire the validity we observed on the old file.
        let stamp = fs::symlink_metadata(&self.listener.path)
            .ok()
            .map(|file| FileStamp::from(&file));
        if stamp.is_some() && stamp == self.stamp {
            return true;
        }
        self.stamp = None;
        if self.listener.is_current() {
            self.stamp = stamp;
            return true;
        }
        false
    }

    pub(crate) fn take(&self, paths: &PlakPaths, session: &str) -> Option<ReloadRequest> {
        self.listener.take(paths, session)
    }
}

type Sources = Vec<(PathBuf, Option<FileStamp>)>;

const FULL_RELOAD_INTERVAL: Duration = Duration::from_mins(3);

#[derive(Default)]
pub(crate) struct ReloadPolicy(Option<(Sources, bool, Instant)>);

impl ReloadPolicy {
    pub(crate) fn enabled(&mut self, paths: &PlakPaths) -> bool {
        self.enabled_at(paths, Instant::now())
    }

    fn enabled_at(&mut self, paths: &PlakPaths, now: Instant) -> bool {
        let stamps = sources(paths).ok();
        if let Some((prior, allow, until)) = &self.0
            && now < *until
            && stamps.as_ref() == Some(prior)
        {
            return *allow;
        }
        let (allow, cacheable) = ProductConfig::reload_policy(paths);
        self.0 = stamps
            .filter(|_| cacheable)
            .map(|stamps| (stamps, allow, now + FULL_RELOAD_INTERVAL));
        allow
    }
}

/// Watch all of the loader's inputs, not just the opt-in key: an invalid
/// template or a newly trusted overlay can change whether the load succeeds.
fn sources(paths: &PlakPaths) -> io::Result<Sources> {
    let mut files = Vec::new();
    stamp(&mut files, paths.config_file())?;
    stamp_templates(&mut files, &paths.user_agents_dir())?;
    if let Some(project) = paths.project_home() {
        stamp(&mut files, paths.trusted_projects_file())?;
        stamp(&mut files, project.join("config.toml"))?;
        stamp_templates(&mut files, &project.join("agents"))?;
    }
    files.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

fn stamp(files: &mut Sources, path: PathBuf) -> io::Result<()> {
    let version = file_stamp(&path)?;
    files.push((path, version));
    Ok(())
}

fn stamp_templates(files: &mut Sources, dir: &Path) -> io::Result<()> {
    stamp(files, dir.to_owned())?;
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            stamp(files, path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "isolated test files")]
mod tests {
    use super::*;

    #[test]
    fn policy_reloads_after_atomic_replacement_and_template_edits() {
        let (_home, paths) = PlakPaths::isolated_test_home();
        fs::create_dir_all(paths.user_agents_dir()).unwrap();
        let config = paths.config_file();
        let mut policy = ReloadPolicy::default();
        fs::write(&config, "[host]\nreload_requests = false\n").unwrap();
        assert!(!policy.enabled(&paths));

        let replacement = config.with_extension("new");
        fs::write(&replacement, "[host]\nreload_requests = true \n").unwrap();
        fs::rename(replacement, &config).unwrap();
        assert!(policy.enabled(&paths));

        let template = paths.user_agents_dir().join("review.toml");
        fs::write(&template, "account = \"codex\"\nmodel = \"example\"\n").unwrap();
        assert!(policy.enabled(&paths));
        fs::write(&template, "not a valid template [").unwrap();
        assert!(
            !policy.enabled(&paths),
            "a malformed template refuses the load"
        );
        assert!(policy.0.is_some(), "a parse failure is cacheable");
        fs::remove_file(template).unwrap();
        assert!(policy.enabled(&paths));
        fs::remove_file(config).unwrap();
        assert!(!policy.enabled(&paths));
    }

    #[test]
    fn unchanged_stamps_expire_and_io_refusals_are_not_cached() {
        let (_home, paths) = PlakPaths::isolated_test_home();
        let config = paths.config_file();
        fs::create_dir_all(&config).unwrap();
        let now = Instant::now();
        let mut policy = ReloadPolicy::default();
        assert!(sources(&paths).is_ok(), "metadata lookup did not fail");
        assert!(
            !policy.enabled_at(&paths, now),
            "a directory cannot be read as configuration"
        );
        assert!(policy.0.is_none(), "the I/O refusal must be tried again");

        fs::remove_dir(&config).unwrap();
        fs::write(&config, "[host]\nreload_requests = true\n").unwrap();
        let stamps = sources(&paths).unwrap();
        let expires = now + FULL_RELOAD_INTERVAL;
        // Simulate a change invisible to the metadata stamp.
        policy.0 = Some((stamps, false, expires));
        assert!(!policy.enabled_at(&paths, now));
        assert!(policy.enabled_at(&paths, expires));
    }

    #[test]
    fn hosted_listener_detects_replacement_and_preserves_the_new_owner() {
        let (_home, paths) = PlakPaths::isolated_test_home();
        fs::create_dir_all(paths.home()).unwrap();
        fs::write(paths.config_file(), "[host]\nreload_requests = true\n").unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let mut old = HostedListener::announce(&paths, &session).unwrap();
        assert!(old.is_current());
        let successor = Listener::announce(&paths, &session).unwrap();
        assert!(!old.is_current());
        drop(old);
        assert!(successor.is_current());
    }
}
