//! Home-local lifecycle operations shared by CLI and host directory dispatch.

use plakous_hosted::error::CliFailure;

use crate::shell::services::Services;

mod fences;
mod forward;
mod keepalive;

pub(crate) fn archive_offline(services: &Services, id: &str) -> Result<(), LifecycleError> {
    fences::archive(services, id, false)
}

pub(crate) fn rename_cli(
    services: &Services,
    id: &str,
    title: Option<&str>,
) -> Result<(), CliFailure> {
    if !forward::request(
        services,
        id,
        forward::Change::Rename(title.map(str::to_owned)),
    )? {
        rename_session(services, id, title)?;
    }
    Ok(())
}

pub(crate) fn archive_cli(services: &Services, id: &str, archived: bool) -> Result<(), CliFailure> {
    if !forward::request(services, id, forward::Change::Archive(archived))? {
        if archived {
            archive_offline(services, id)?;
        } else {
            set_archived(services, id, false)?;
        }
    }
    Ok(())
}

pub(super) fn forward_delete(
    services: &Services,
    id: &str,
    abandon_children: bool,
) -> Result<bool, CliFailure> {
    forward::request(services, id, forward::Change::Delete(abandon_children))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InvalidTitleKind {
    Empty,
    TooLong,
    ControlCharacter,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LifecycleRefusal {
    NotFound,
    InvalidTitle {
        kind: InvalidTitleKind,
    },
    Busy {
        turn: bool,
        children: Vec<String>,
        children_truncated: bool,
        processes: Vec<String>,
        processes_truncated: bool,
        state_unknown: bool,
    },
    Open,
    LiveTree {
        children: Vec<String>,
        children_truncated: bool,
    },
    RecoveryRequiredTree {
        children: Vec<String>,
        children_truncated: bool,
    },
    ForkDependency {
        dependents: Vec<String>,
        dependents_truncated: bool,
    },
}

#[derive(Debug)]
pub(crate) enum LifecycleError {
    Refused(LifecycleRefusal),
    Failure(CliFailure),
}

impl LifecycleRefusal {
    pub(super) fn live_tree(ids: Vec<String>) -> Self {
        let (children, children_truncated) = bounded_ids(ids);
        Self::LiveTree {
            children,
            children_truncated,
        }
    }

    pub(super) fn recovery_required_tree(ids: Vec<String>) -> Self {
        let (children, children_truncated) = bounded_ids(ids);
        Self::RecoveryRequiredTree {
            children,
            children_truncated,
        }
    }

    pub(super) fn fork_dependency(ids: Vec<String>) -> Self {
        let (dependents, dependents_truncated) = bounded_ids(ids);
        Self::ForkDependency {
            dependents,
            dependents_truncated,
        }
    }
}

impl From<CliFailure> for LifecycleError {
    fn from(error: CliFailure) -> Self {
        Self::Failure(error)
    }
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for LifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Failure(error) => Some(error),
            Self::Refused(_) => None,
        }
    }
}

impl From<LifecycleError> for CliFailure {
    fn from(error: LifecycleError) -> Self {
        match error {
            LifecycleError::Failure(error) => error,
            LifecycleError::Refused(reason) => {
                Self::new(format!("session lifecycle refused: {reason}"))
            }
        }
    }
}

impl LifecycleError {
    pub(super) fn message(&self) -> String {
        match self {
            Self::Failure(error) => error.message().to_owned(),
            Self::Refused(reason) => format!("session lifecycle refused: {reason}"),
        }
    }
}

impl std::fmt::Display for LifecycleRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str(
                "the session was not found; refresh the session list and choose an existing id",
            ),
            Self::InvalidTitle { kind } => f.write_str(match kind {
                InvalidTitleKind::Empty => {
                    "the title is blank; supply a nonblank title or use --clear"
                }
                InvalidTitleKind::TooLong => {
                    "the title is too long; use at most 128 Unicode scalars and 512 UTF-8 bytes"
                }
                InvalidTitleKind::ControlCharacter => {
                    "the title contains control characters; remove them before retrying"
                }
            }),
            Self::Busy {
                turn,
                children,
                processes,
                state_unknown,
                ..
            } => write!(
                f,
                "the session has unfinished work (turn or continuation: {turn}, children: {}, processes: {}, activity unknown: {state_unknown}); resume or stop outstanding work, or finish interrupted deletion, then retry",
                children.join(", "),
                processes.join(", ")
            ),
            Self::Open => f.write_str("it is open in a running plak; close it there first"),
            Self::LiveTree { children, .. } => write!(
                f,
                "its agent tree has child agents still running or resumable ({}); close them first",
                children.join(", ")
            ),
            Self::RecoveryRequiredTree { children, .. } => write!(
                f,
                "its agent tree has child agents that need recovery ({}); recover them first, or delete with --abandon-children only if this session owns their tree",
                children.join(", ")
            ),
            Self::ForkDependency { dependents, .. } => write!(
                f,
                "sessions {} continue from it or share its runtime history; finish their carry or delete the dependent sessions first",
                dependents.join(", ")
            ),
        }
    }
}

pub(crate) fn validate_title(title: Option<&str>) -> Result<(), LifecycleError> {
    let Some(title) = title else {
        return Ok(());
    };
    let kind = if title.chars().any(char::is_control) {
        Some(InvalidTitleKind::ControlCharacter)
    } else if title.trim().is_empty() {
        Some(InvalidTitleKind::Empty)
    } else if title.len() > 512 || title.chars().count() > 128 {
        Some(InvalidTitleKind::TooLong)
    } else {
        None
    };
    match kind {
        Some(kind) => Err(LifecycleError::Refused(LifecycleRefusal::InvalidTitle {
            kind,
        })),
        None => Ok(()),
    }
}

/// Evidence sampled while the caller holds the entire tree's admission barrier.
/// An incomplete probe must set `state_unknown`; it never proves quiescence.
#[derive(Debug, Default)]
struct SessionActivity {
    turn: bool,
    children: Vec<String>,
    processes: Vec<String>,
    state_unknown: bool,
}

impl SessionActivity {
    fn refuse_running(&self) -> Result<(), LifecycleError> {
        if !self.turn
            && self.children.is_empty()
            && self.processes.is_empty()
            && !self.state_unknown
        {
            return Ok(());
        }
        let (children, children_truncated) = bounded_ids(self.children.clone());
        let (processes, processes_truncated) = bounded_ids(self.processes.clone());
        Err(LifecycleError::Refused(LifecycleRefusal::Busy {
            turn: self.turn,
            children,
            children_truncated,
            processes,
            processes_truncated,
            state_unknown: self.state_unknown,
        }))
    }
}

pub(super) fn bounded_ids(mut ids: Vec<String>) -> (Vec<String>, bool) {
    let truncated = ids.len() > 100;
    ids.truncate(100);
    (ids, truncated)
}

pub(crate) fn rename_session(
    services: &Services,
    id: &str,
    title: Option<&str>,
) -> Result<Option<String>, LifecycleError> {
    validate_title(title)?;
    let paths = services.paths()?;
    let mut store = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    store.set_title_override(id, title)
}

/// The host holds a nonresident session's slot through this call. Existing
/// branches are fenced here; the slot also serializes a first-create reservation.
/// Unarchiving starts no work; repeated state is an acknowledged no-op.
pub(crate) fn set_archived(
    services: &Services,
    id: &str,
    archived: bool,
) -> Result<(), LifecycleError> {
    if archived {
        return fences::archive(services, id, true);
    }
    let paths = services.paths()?;
    let mut store = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let previous = store.lifecycle_metadata(id)?;
    if previous.1 == archived {
        return Ok(());
    }
    store.set_archived(id, archived)
}

/// Already confirmed by the operator. The host must release an idle resident
/// owner and hold admission until this returns; busy owners are never stopped.
pub(crate) fn delete_confirmed(
    services: &Services,
    id: &str,
    abandon_children: bool,
) -> Result<(), LifecycleError> {
    super::delete::delete_confirmed(services, id, abandon_children)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "lifecycle contract tests")]
mod tests {
    use super::*;

    #[test]
    fn missing_sessions_refuse_every_lifecycle_operation_without_creating_a_record() {
        let (_home, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        let services = Services::for_home(paths.clone());
        let id = uuid::Uuid::new_v4().to_string();
        for result in [
            rename_session(&services, &id, None).map(|_| ()),
            set_archived(&services, &id, true),
            set_archived(&services, &id, false),
            delete_confirmed(&services, &id, false),
        ] {
            assert!(matches!(
                result,
                Err(LifecycleError::Refused(LifecycleRefusal::NotFound))
            ));
        }
        assert_eq!(
            crate::store::ProductStore::open(&paths)
                .unwrap()
                .directory(None)
                .unwrap(),
            []
        );
        assert!(!paths.runtime_database().exists());
    }

    #[test]
    fn titles_are_validated_without_normalizing_and_null_clears() {
        for (title, expected) in [
            ("", InvalidTitleKind::Empty),
            (" \u{2003}", InvalidTitleKind::Empty),
            ("label\n", InvalidTitleKind::ControlCharacter),
            ("a\u{7f}", InvalidTitleKind::ControlCharacter),
        ] {
            assert!(
                matches!(validate_title(Some(title)), Err(LifecycleError::Refused(LifecycleRefusal::InvalidTitle {kind})) if kind == expected)
            );
        }
        assert!(matches!(
            validate_title(Some(&"a".repeat(129))),
            Err(LifecycleError::Refused(LifecycleRefusal::InvalidTitle {
                kind: InvalidTitleKind::TooLong
            }))
        ));
        validate_title(Some(&"🦀".repeat(128))).unwrap();
        validate_title(Some("  label  ")).unwrap();
        validate_title(None).unwrap();
    }

    #[test]
    fn archive_refuses_each_running_dimension_and_unknown_evidence() {
        for activity in [
            SessionActivity {
                turn: true,
                children: vec![],
                processes: vec![],
                state_unknown: false,
            },
            SessionActivity {
                turn: false,
                children: vec!["child".into()],
                processes: vec![],
                state_unknown: false,
            },
            SessionActivity {
                turn: false,
                children: vec![],
                processes: vec!["process".into()],
                state_unknown: false,
            },
            SessionActivity {
                turn: false,
                children: vec![],
                processes: vec![],
                state_unknown: true,
            },
        ] {
            assert!(matches!(
                activity.refuse_running(),
                Err(LifecycleError::Refused(LifecycleRefusal::Busy { .. }))
            ));
        }
        SessionActivity {
            turn: false,
            children: vec![],
            processes: vec![],
            state_unknown: false,
        }
        .refuse_running()
        .unwrap();
        let activity = SessionActivity {
            turn: false,
            children: (0..101).map(|id| id.to_string()).collect(),
            processes: vec![],
            state_unknown: false,
        };
        assert!(
            matches!(activity.refuse_running(),Err(LifecycleError::Refused(LifecycleRefusal::Busy{children,children_truncated:true,..})) if children.len()==100)
        );
    }

    #[test]
    fn rename_and_archive_operate_without_a_runtime_and_preserve_generated_title() {
        let (_home, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        let services = Services::for_home(paths.clone());
        let mut store = crate::store::ProductStore::open(&paths).unwrap();
        let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_owned());
        record.title = Some("generated".into());
        store.commit(&mut record).unwrap();
        assert!(matches!(
            archive_offline(&services, &record.id),
            Err(LifecycleError::Refused(LifecycleRefusal::Busy {
                state_unknown: true,
                ..
            }))
        ));
        assert!(!paths.runtime_database().exists());
        rename_session(&services, &record.id, Some("  label  ")).unwrap();
        set_archived(&services, &record.id, true).unwrap();
        rename_session(&services, &record.id, Some("archived label")).unwrap();
        set_archived(&services, &record.id, true).unwrap();
        let loaded = store.load(&record.id).unwrap();
        assert_eq!(loaded.title.as_deref(), Some("generated"));
        assert_eq!(loaded.title_override.as_deref(), Some("archived label"));
        assert!(loaded.archived);
        assert_eq!(store.directory(None).unwrap(), []);
        assert_eq!(
            store.directory_with_archived(None, true).unwrap()[0]
                .title
                .as_deref(),
            Some("archived label")
        );
        assert_eq!(
            rename_session(&services, &record.id, None).unwrap(),
            Some("generated".into())
        );
        set_archived(&services, &record.id, false).unwrap();
        assert_eq!(
            store.directory(None).unwrap()[0].title.as_deref(),
            Some("generated")
        );
        assert!(store.load(&record.id).unwrap().plakous_locator.is_none());
        delete_confirmed(&services, &record.id, false).unwrap();
        assert!(matches!(
            delete_confirmed(&services, &record.id, false),
            Err(LifecycleError::Refused(LifecycleRefusal::NotFound))
        ));
    }
}
