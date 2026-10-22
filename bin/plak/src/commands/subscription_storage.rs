//! Names the path and the rule behind a refused subscription store.
//!
//! The facade reports `UnsafeStorage` without a location. The product
//! re-inspects the state root's ancestry and the fixed names the store opens
//! under it, against the store's own rules (spec 04 §3.2), so the operator
//! learns what to repair. The inspection is read-only, follows no symbolic
//! link and ignores every name the store never opens.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use plakous::subscriptions::NativeProvider;

/// One store rule a path on disk breaks.
#[derive(Clone, Debug, Eq, PartialEq)]
enum StorageRule {
    /// The store refuses a state root with `.` or `..` components before
    /// touching the filesystem.
    NotNormal,
    SymbolicLink,
    NotDirectory,
    NotRegularFile,
    Owner {
        found: u32,
        expected: u32,
    },
    /// The nearest existing parent of a missing state root cannot receive the
    /// directories the store would create below it.
    NotWritable,
    /// The nearest existing parent of a missing state root is setgid, so on
    /// Linux the directories created below it would not be exactly 0700.
    #[cfg(target_os = "linux")]
    SetgidParent {
        mode: u32,
    },
    /// An ancestor the store opens with `O_RDONLY | O_DIRECTORY` that the
    /// current user cannot read and search.
    NoAccess,
    OwnerAccess {
        mode: u32,
    },
    RootWritable {
        mode: u32,
    },
    SpecialBits {
        mode: u32,
    },
    DirectoryMode {
        mode: u32,
    },
    FileMode {
        mode: u32,
    },
    HardLinks {
        links: u64,
    },
    Uninspectable,
}

/// Where a path sits relative to what Plak created, which bounds the repair
/// it may recommend: nothing outside the subscription store is replaced or
/// re-owned on Plak's say-so.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Place {
    /// The state root, its ancestors and `<root>/plak`.
    Outside,
    /// `<root>/plak/subscriptions` and the fixed names below it.
    Store,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StorageViolation {
    path: PathBuf,
    rule: StorageRule,
    place: Place,
}

const RELOCATE: &str = "set XDG_STATE_HOME to a directory you own whose path has no symbolic links";

const CREDENTIAL: &str = "credential.json";

impl StorageViolation {
    /// How to get rid of an entry inside the store. Plak recreates every
    /// store entry except a credential, which only a login writes.
    fn move_aside(&self) -> String {
        let account = self
            .path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy());
        match account {
            Some(account) if self.path.file_name() == Some(CREDENTIAL.as_ref()) => {
                format!("move it aside, then run `plak login {account}`")
            }
            _ => "move it aside so Plak can recreate it".to_owned(),
        }
    }
}

impl fmt::Display for StorageViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        let quoted = shell_quote(&self.path);
        let outside = self.place == Place::Outside;
        match &self.rule {
            StorageRule::NotNormal => write!(
                formatter,
                "{path} contains a `.` or `..` component, which credential storage refuses; set XDG_STATE_HOME to an absolute path without them"
            ),
            StorageRule::SymbolicLink if outside => write!(
                formatter,
                "{path} is a symbolic link and credential storage follows no links; {RELOCATE}"
            ),
            StorageRule::SymbolicLink => write!(
                formatter,
                "{path} is a symbolic link and credential storage follows no links; {}",
                self.move_aside()
            ),
            StorageRule::NotDirectory if outside => {
                write!(formatter, "{path} is not a directory; {RELOCATE}")
            }
            StorageRule::NotDirectory => write!(
                formatter,
                "{path} is not a directory; {}",
                self.move_aside()
            ),
            StorageRule::NotRegularFile => write!(
                formatter,
                "{path} is not a regular file; {}",
                self.move_aside()
            ),
            StorageRule::Owner { found, expected } if outside => write!(
                formatter,
                "{path} is owned by uid {found}, not the current user (uid {expected}); {RELOCATE}"
            ),
            StorageRule::Owner { found, expected } => write!(
                formatter,
                "{path} is owned by uid {found}, not the current user (uid {expected}); `chown` it back to the current user"
            ),
            StorageRule::NotWritable => write!(
                formatter,
                "{path} is not a directory the current user can read, write and search, so the state directory below it cannot be created; {RELOCATE}"
            ),
            #[cfg(target_os = "linux")]
            StorageRule::SetgidParent { mode } => write!(
                formatter,
                "{path} has mode {mode:04o} with the setgid bit, so directories created below it would not be private; {RELOCATE}"
            ),
            StorageRule::NoAccess => write!(
                formatter,
                "{path} does not let the current user read and search it; {RELOCATE}"
            ),
            StorageRule::OwnerAccess { mode } => write!(
                formatter,
                "{path} has mode {mode:04o} but its owner needs read, write and search access; run `chmod u+rwx {quoted}`"
            ),
            StorageRule::RootWritable { mode } => write!(
                formatter,
                "{path} has mode {mode:04o}, writable by group or others; run `chmod go-w {quoted}`"
            ),
            // The state root may be shared with other tools, so only the bits
            // the store refuses are cleared.
            StorageRule::SpecialBits { mode } => write!(
                formatter,
                "{path} has mode {mode:04o} with setuid, setgid or sticky bits the store refuses; run `chmod a-st {quoted}`"
            ),
            // GNU chmod keeps a directory's setuid and setgid bits under a
            // plain numeric mode; the extra leading zero clears them there and
            // means the same on BSD.
            StorageRule::DirectoryMode { mode } => write!(
                formatter,
                "{path} has mode {mode:04o} but private directories must be 0700; run `chmod {} {quoted}`",
                if mode & 0o7000 == 0 { "700" } else { "00700" }
            ),
            StorageRule::FileMode { mode } => write!(
                formatter,
                "{path} has mode {mode:04o} but credential files must be 0600; run `chmod 600 {quoted}`"
            ),
            StorageRule::HardLinks { links } => write!(
                formatter,
                "{path} has {links} hard links but a credential file must have exactly one; remove the other links to it"
            ),
            StorageRule::Uninspectable if outside => {
                write!(formatter, "{path} could not be inspected; {RELOCATE}")
            }
            StorageRule::Uninspectable => write!(
                formatter,
                "{path} could not be inspected; restore the current user's access to it"
            ),
        }
    }
}

/// `path` as one POSIX shell word.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// The first rule the subscription store under `state_home` breaks, or `None`
/// when every path the store opens passes. With an `account`, only that
/// account's files are inspected, since no other account's file can refuse it.
#[cfg(unix)]
pub(crate) fn find_violation(
    state_home: &Path,
    account: Option<NativeProvider>,
) -> Option<StorageViolation> {
    use rustix::fs::Access;
    let euid = rustix::process::geteuid().as_raw();
    if state_home.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Some(outside(state_home, StorageRule::NotNormal));
    }
    let mut parent: Option<&Path> = None;
    for ancestor in state_home.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Some(outside(ancestor, StorageRule::SymbolicLink));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Some(outside(ancestor, StorageRule::NotDirectory));
            }
            // The store opens each ancestor `O_RDONLY | O_DIRECTORY` on its
            // way down; the root's own access is `check_state_root`'s rule.
            Ok(_)
                if ancestor != state_home
                    && !accessible(ancestor, Access::READ_OK | Access::EXEC_OK) =>
            {
                return Some(outside(ancestor, StorageRule::NoAccess));
            }
            Ok(_) => parent = Some(ancestor),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return parent.and_then(check_creatable);
            }
            Err(_) => return Some(outside(parent.unwrap_or(ancestor), StorageRule::NoAccess)),
        }
    }
    if let Some(found) = check_state_root(state_home, euid) {
        return Some(found);
    }
    let plak = state_home.join("plak");
    let subscriptions = plak.join("subscriptions");
    let mut entries = vec![
        (plak, Place::Outside, Kind::Directory),
        (subscriptions.clone(), Place::Store, Kind::Directory),
    ];
    let mut providers: Vec<_> = plakous_hosted::capabilities::native_providers().collect();
    if let Some(account) = account {
        providers.retain(|provider| *provider != account);
        providers.insert(0, account);
    }
    for directory in providers
        .iter()
        .map(|provider| provider.as_str())
        .chain(["catalog", "compatibility"])
    {
        entries.push((subscriptions.join(directory), Place::Store, Kind::Directory));
    }
    // Every account directory is opened on every operation, but an account's
    // files only by that account: another account's file is never the cause.
    let file_owners = match account {
        Some(account) => vec![account],
        None => providers,
    };
    for provider in file_owners {
        entries.extend(
            store_files(&subscriptions, provider)
                .into_iter()
                .map(|path| (path, Place::Store, Kind::File)),
        );
    }
    entries
        .iter()
        .find_map(|(path, place, kind)| check_entry(path, *place, *kind, euid))
}

#[cfg(not(unix))]
pub(crate) fn find_violation(
    _state_home: &Path,
    _account: Option<NativeProvider>,
) -> Option<StorageViolation> {
    None
}

/// The files the store opens for `provider` under `subscriptions`.
fn store_files(subscriptions: &Path, provider: NativeProvider) -> Vec<PathBuf> {
    let name = provider.as_str();
    let compatibility = subscriptions.join("compatibility");
    let account = subscriptions.join(name);
    let mut files = vec![
        account.join(CREDENTIAL),
        account.join(".credential.json.lock"),
        account.join(".credential.json.refresh.lock"),
        compatibility.join(format!("{name}.json")),
        compatibility.join(format!(".{name}.lock")),
        compatibility.join(format!("{name}.login-recovery")),
        compatibility.join(format!("{name}.login-recovery-epoch")),
        subscriptions.join("catalog").join(format!("{name}.json")),
    ];
    if provider == NativeProvider::Claude {
        files.push(subscriptions.join("client-device-id"));
        files.push(subscriptions.join(".client-device-id.lock"));
    }
    files
}

fn outside(path: &Path, rule: StorageRule) -> StorageViolation {
    StorageViolation {
        path: path.to_path_buf(),
        rule,
        place: Place::Outside,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Directory,
    File,
}

/// Whether the effective user has `access` to `path`.
#[cfg(unix)]
fn accessible(path: &Path, access: rustix::fs::Access) -> bool {
    rustix::fs::accessat(rustix::fs::CWD, path, access, rustix::fs::AtFlags::EACCESS).is_ok()
}

/// The store creates a missing state root one component at a time from its
/// nearest existing parent, so that parent must let the current user open,
/// create and enter entries, and must not hand its group to them.
#[cfg(unix)]
fn check_creatable(parent: &Path) -> Option<StorageViolation> {
    use rustix::fs::Access;
    if !accessible(parent, Access::READ_OK | Access::WRITE_OK | Access::EXEC_OK) {
        return Some(outside(parent, StorageRule::NotWritable));
    }
    // BSD never propagates setgid to new directories; Linux does.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt as _;
        let mode = fs::symlink_metadata(parent).ok()?.mode() & 0o7777;
        if mode & 0o2000 != 0 {
            return Some(outside(parent, StorageRule::SetgidParent { mode }));
        }
    }
    None
}

/// The state root may predate Plak: it must be a directory the effective user
/// owns with full owner access, no group or other write, and no special bits.
#[cfg(unix)]
fn check_state_root(path: &Path, euid: u32) -> Option<StorageViolation> {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Some(outside(path, StorageRule::Uninspectable));
    };
    if metadata.uid() != euid {
        return Some(outside(
            path,
            StorageRule::Owner {
                found: metadata.uid(),
                expected: euid,
            },
        ));
    }
    let mode = metadata.mode() & 0o7777;
    let rule = if mode & 0o700 != 0o700 {
        StorageRule::OwnerAccess { mode }
    } else if mode & 0o022 != 0 {
        StorageRule::RootWritable { mode }
    } else if mode & !0o777 != 0 {
        StorageRule::SpecialBits { mode }
    } else {
        return None;
    };
    Some(outside(path, rule))
}

/// A store directory must be a real 0700 directory and a store file a real
/// 0600 file with one link, both owned by the effective user. A path that
/// does not exist breaks no rule: the store creates it.
#[cfg(unix)]
fn check_entry(path: &Path, place: Place, kind: Kind, euid: u32) -> Option<StorageViolation> {
    use std::os::unix::fs::MetadataExt as _;
    let found = |rule| {
        Some(StorageViolation {
            path: path.to_path_buf(),
            rule,
            place,
        })
    };
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return found(StorageRule::Uninspectable),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return found(StorageRule::SymbolicLink);
    }
    match kind {
        Kind::Directory if !file_type.is_dir() => return found(StorageRule::NotDirectory),
        Kind::File if !file_type.is_file() => return found(StorageRule::NotRegularFile),
        Kind::Directory | Kind::File => {}
    }
    if metadata.uid() != euid {
        return found(StorageRule::Owner {
            found: metadata.uid(),
            expected: euid,
        });
    }
    let mode = metadata.mode() & 0o7777;
    match kind {
        Kind::Directory if mode != 0o700 => found(StorageRule::DirectoryMode { mode }),
        Kind::Directory => None,
        Kind::File if mode != 0o600 => found(StorageRule::FileMode { mode }),
        Kind::File if metadata.nlink() != 1 => found(StorageRule::HardLinks {
            links: metadata.nlink(),
        }),
        Kind::File => None,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn inside(path: &Path, rule: StorageRule) -> StorageViolation {
        StorageViolation {
            path: path.to_path_buf(),
            rule,
            place: Place::Store,
        }
    }

    fn private_layout() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(home.path()).unwrap().join("state");
        let subscriptions = root.join("plak/subscriptions");
        for name in ["codex", "catalog", "compatibility"] {
            fs::create_dir_all(subscriptions.join(name)).unwrap();
        }
        for directory in [
            root.clone(),
            root.join("plak"),
            subscriptions.clone(),
            subscriptions.join("codex"),
            subscriptions.join("catalog"),
            subscriptions.join("compatibility"),
        ] {
            set_mode(&directory, 0o700);
        }
        let record = subscriptions.join("codex/credential.json");
        fs::write(&record, b"{}").unwrap();
        set_mode(&record, 0o600);
        (home, root)
    }

    #[test]
    fn a_private_layout_breaks_no_rule() {
        let (_home, root) = private_layout();
        assert_eq!(find_violation(&root, None), None);
        assert_eq!(find_violation(&root.join("absent/state"), None), None);
    }

    #[test]
    fn each_broken_rule_names_its_path_and_the_repair() {
        let (_home, root) = private_layout();
        let codex = root.join("plak/subscriptions/codex");
        set_mode(&codex, 0o755);
        let found = find_violation(&root, None).unwrap();
        assert_eq!(
            found,
            inside(&codex, StorageRule::DirectoryMode { mode: 0o755 })
        );
        assert_eq!(
            found.to_string(),
            format!(
                "{0} has mode 0755 but private directories must be 0700; run `chmod 700 '{0}'`",
                codex.display()
            )
        );
        set_mode(&codex, 0o700);

        let record = codex.join("credential.json");
        set_mode(&record, 0o644);
        assert_eq!(
            find_violation(&root, None),
            Some(inside(&record, StorageRule::FileMode { mode: 0o644 }))
        );
        set_mode(&record, 0o600);

        let alias = root.join("alias");
        fs::hard_link(&record, &alias).unwrap();
        let found = find_violation(&root, None).unwrap();
        assert_eq!(found, inside(&record, StorageRule::HardLinks { links: 2 }));
        assert!(found.to_string().contains("exactly one"));
        fs::remove_file(&alias).unwrap();

        let link = root.join("plak/subscriptions/catalog/codex.json");
        std::os::unix::fs::symlink(&record, &link).unwrap();
        let found = find_violation(&root, None).unwrap();
        assert_eq!(found, inside(&link, StorageRule::SymbolicLink));
        assert!(found.to_string().contains("move it aside"));
        fs::remove_file(&link).unwrap();

        set_mode(&root, 0o777);
        assert_eq!(
            find_violation(&root, None),
            Some(outside(&root, StorageRule::RootWritable { mode: 0o777 }))
        );
    }

    #[test]
    fn the_state_root_follows_the_stores_own_rules() {
        let (_home, root) = private_layout();
        // The store admits a state root other tools share: group and other
        // may read and search it.
        set_mode(&root, 0o755);
        assert_eq!(find_violation(&root, None), None);

        set_mode(&root, 0o500);
        let found = find_violation(&root, None).unwrap();
        assert_eq!(
            found,
            outside(&root, StorageRule::OwnerAccess { mode: 0o500 })
        );
        assert!(
            found
                .to_string()
                .ends_with(&format!("run `chmod u+rwx '{}'`", root.display()))
        );

        set_mode(&root, 0o1700);
        let found = find_violation(&root, None).unwrap();
        assert_eq!(
            found,
            outside(&root, StorageRule::SpecialBits { mode: 0o1700 })
        );
        assert!(found.to_string().contains("sticky"));
        assert!(
            found
                .to_string()
                .ends_with(&format!("run `chmod a-st '{}'`", root.display()))
        );
        set_mode(&root, 0o700);
    }

    #[test]
    fn only_the_names_the_store_opens_are_inspected() {
        let (_home, root) = private_layout();
        let subscriptions = root.join("plak/subscriptions");
        let stray = subscriptions.join(".DS_Store");
        fs::write(&stray, b"finder").unwrap();
        set_mode(&stray, 0o644);
        assert_eq!(find_violation(&root, None), None);

        let lock = subscriptions.join("compatibility/.codex.lock");
        fs::write(&lock, b"").unwrap();
        set_mode(&lock, 0o644);
        assert_eq!(
            find_violation(&root, None),
            Some(inside(&lock, StorageRule::FileMode { mode: 0o644 }))
        );
    }

    #[test]
    fn another_accounts_files_are_never_blamed() {
        let (_home, root) = private_layout();
        let compatibility = root.join("plak/subscriptions/compatibility");
        let codex = compatibility.join("codex.json");
        let claude = compatibility.join("claude.json");
        for receipt in [&codex, &claude] {
            fs::write(receipt, b"{}").unwrap();
            set_mode(receipt, 0o640);
        }
        assert_eq!(
            find_violation(&root, None).map(|found| found.path),
            Some(codex.clone())
        );
        assert_eq!(
            find_violation(&root, Some(NativeProvider::Claude)).map(|found| found.path),
            Some(claude)
        );
        assert_eq!(find_violation(&root, Some(NativeProvider::Xai)), None);
    }

    #[test]
    fn the_credential_locks_are_inspected_and_a_credential_needs_a_login() {
        let (_home, root) = private_layout();
        let codex = root.join("plak/subscriptions/codex");
        for name in [".credential.json.lock", ".credential.json.refresh.lock"] {
            let lock = codex.join(name);
            fs::write(&lock, b"").unwrap();
            set_mode(&lock, 0o644);
            assert_eq!(
                find_violation(&root, Some(NativeProvider::Codex)),
                Some(inside(&lock, StorageRule::FileMode { mode: 0o644 }))
            );
            set_mode(&lock, 0o600);
        }

        let record = codex.join("credential.json");
        fs::remove_file(&record).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), &record).unwrap();
        let found = find_violation(&root, Some(NativeProvider::Codex)).unwrap();
        assert_eq!(found, inside(&record, StorageRule::SymbolicLink));
        assert!(
            found
                .to_string()
                .ends_with("move it aside, then run `plak login codex`"),
            "{found}"
        );
    }

    #[test]
    fn an_ancestor_the_user_cannot_read_gets_a_relocation_remedy() {
        let (home, _root) = private_layout();
        let closed = fs::canonicalize(home.path()).unwrap().join("closed");
        fs::create_dir_all(closed.join("state")).unwrap();
        set_mode(&closed.join("state"), 0o700);
        set_mode(&closed, 0o100);
        let found = find_violation(&closed.join("state"), None);
        set_mode(&closed, 0o700);
        // Root bypasses permission bits.
        if rustix::process::geteuid().is_root() {
            return;
        }
        let found = found.unwrap();
        assert_eq!(found, outside(&closed, StorageRule::NoAccess));
        assert!(found.to_string().ends_with(RELOCATE), "{found}");
    }

    #[test]
    fn paths_plak_did_not_create_get_only_a_relocation_remedy() {
        let (home, root) = private_layout();
        let linked = fs::canonicalize(home.path()).unwrap().join("linked");
        std::os::unix::fs::symlink(&root, &linked).unwrap();
        let found = find_violation(&linked.join("state"), None).unwrap();
        assert_eq!(found, outside(&linked, StorageRule::SymbolicLink));
        assert!(found.to_string().ends_with(RELOCATE), "{found}");
        assert!(!found.to_string().contains("replace"), "{found}");

        let file = fs::canonicalize(home.path()).unwrap().join("file");
        fs::write(&file, b"").unwrap();
        let found = find_violation(&file.join("state"), None).unwrap();
        assert_eq!(found, outside(&file, StorageRule::NotDirectory));
        assert!(found.to_string().ends_with(RELOCATE), "{found}");

        let foreign = outside(
            Path::new("/var/lib/x"),
            StorageRule::Owner {
                found: 0,
                expected: 501,
            },
        );
        assert_eq!(
            foreign.to_string(),
            format!("/var/lib/x is owned by uid 0, not the current user (uid 501); {RELOCATE}")
        );
        assert_eq!(
            inside(
                Path::new("/state/plak/subscriptions/codex"),
                StorageRule::Owner {
                    found: 0,
                    expected: 501,
                },
            )
            .to_string(),
            "/state/plak/subscriptions/codex is owned by uid 0, not the current user (uid 501); `chown` it back to the current user"
        );
    }

    #[test]
    fn a_missing_state_root_needs_a_writable_parent() {
        let (home, _root) = private_layout();
        let parent = fs::canonicalize(home.path()).unwrap().join("sealed");
        fs::create_dir(&parent).unwrap();
        set_mode(&parent, 0o500);
        let found = find_violation(&parent.join("state"), None);
        set_mode(&parent, 0o700);
        // Root bypasses permission bits, so the refusal only exists for an
        // ordinary user.
        if rustix::process::geteuid().is_root() {
            return;
        }
        let found = found.unwrap();
        assert_eq!(found, outside(&parent, StorageRule::NotWritable));
        assert!(found.to_string().ends_with(RELOCATE), "{found}");
    }

    #[test]
    fn a_state_root_with_dot_dot_is_named_before_the_filesystem() {
        let (_home, root) = private_layout();
        let detour = root.join("plak/../state");
        let found = find_violation(&detour, None).unwrap();
        assert_eq!(found, outside(&detour, StorageRule::NotNormal));
        assert!(found.to_string().contains("without them"), "{found}");
    }

    #[test]
    fn remedies_quote_paths_for_the_shell() {
        let found = inside(
            Path::new("/state/it's/plak/subscriptions"),
            StorageRule::DirectoryMode { mode: 0o2700 },
        );
        assert_eq!(
            found.to_string(),
            r"/state/it's/plak/subscriptions has mode 2700 but private directories must be 0700; run `chmod 00700 '/state/it'\''s/plak/subscriptions'`"
        );
    }
}
