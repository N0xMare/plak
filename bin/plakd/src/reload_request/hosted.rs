use std::io;
use std::path::{Path, PathBuf};

use crate::product_config::metadata::{FileStamp, file_stamp};

use super::{
    Listener, PlakPaths, ReloadRequest, enabled, fs, listener_path, private_directory,
    read_announcement,
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

#[derive(Default)]
pub(crate) struct ReloadPolicy(Option<(Sources, bool)>);

impl ReloadPolicy {
    pub(crate) fn enabled(&mut self, paths: &PlakPaths) -> bool {
        let Ok(sources) = sources(paths) else {
            self.0 = None;
            return enabled(paths);
        };
        if let Some((prior, allow)) = &self.0
            && *prior == sources
        {
            return *allow;
        }
        let allow = enabled(paths);
        self.0 = Some((sources, allow));
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
        fs::remove_file(template).unwrap();
        assert!(policy.enabled(&paths));
        fs::remove_file(config).unwrap();
        assert!(!policy.enabled(&paths));
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
