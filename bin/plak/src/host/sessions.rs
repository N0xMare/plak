//! The product side of `plak host`'s session directory (spec 02 §4.4,
//! §4.6): the rows its stores show, the newest session of a tree, a
//! session's open in its own workspace, and an open's refusal.
//!
//! A row's `hosted_elsewhere` comes from its tree's custody after the
//! sealed liveness check (`holder_elsewhere`): a crashed host's or TUI's
//! stale custody never reads as held, and a holder on another machine,
//! which cannot be proven gone, always does.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use plakous_host_protocol::wire::{
    HostedElsewhere, NodeAddress, RequestRefusal, SessionActivity, SessionEnvironment,
    SessionResidency, SessionRow,
};
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::host_service::AttachmentCount;
use plakous_hosted::host_service::wire::{Created, HostSessions, OpenedSession, session_root_of};
use plakous_hosted::runtime::{IDLE_RELEASE, IdleRelease, RootStanding, spawn_host_worker};
use plakous_recursive_subagents::tree::holder_elsewhere;
use plakous_recursive_subagents_store::SqliteTreeStore;

use crate::shell::services::Services;
use crate::store::ProductStore;
use crate::store::directory::DirectoryEntry;
use crate::surface_protocol::PlakExtension;

/// This home's sessions, as `plak host` serves them.
pub(super) struct ProductSessions {
    services: Services,
    idle_release: Option<std::time::Duration>,
    product: Mutex<Option<ProductStore>>,
    tree: SqliteTreeStore,
}

impl ProductSessions {
    /// The sessions of `services`' home.
    ///
    /// # Errors
    ///
    /// The tree store does not open.
    pub(super) fn new(
        services: &Services,
        idle_release: Option<std::time::Duration>,
    ) -> CliResult<Self> {
        let tree =
            crate::store::tree::open_tree_store(services.paths()?).map_err(CliFailure::new)?;
        Ok(Self {
            services: services.clone(),
            idle_release,
            product: Mutex::new(None),
            tree,
        })
    }

    fn with_product<T>(
        &self,
        read: impl FnOnce(&mut ProductStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut product = self.product.lock().unwrap_or_else(PoisonError::into_inner);
        if product.is_none() {
            let paths = self
                .services
                .paths()
                .map_err(|failure| failure.message().to_owned())?;
            *product = Some(ProductStore::open(paths).map_err(|error| error.to_string())?);
        }
        let store = product
            .as_mut()
            .ok_or_else(|| "the product store is unavailable".to_owned())?;
        read(store)
    }

    /// The live host process other than this one that holds `tree_id`.
    fn elsewhere(&self, tree_id: &str) -> Option<HostedElsewhere> {
        holder_elsewhere(&self.tree, tree_id)
            .ok()
            .flatten()
            .map(|holder| HostedElsewhere {
                host_name: holder.host_name,
                binary: holder.binary,
                pid: holder.instance.pid(),
            })
    }

    fn row(&self, entry: DirectoryEntry) -> SessionRow {
        let activity = self
            .services
            .session_records()
            .ok()
            .and_then(|store| {
                plakous_hosted::runtime::root_standing(store.as_ref(), &entry.id).ok()
            })
            .map_or(SessionActivity::Idle, activity_of);
        SessionRow {
            hosted_elsewhere: self.elsewhere(&entry.node.tree_id),
            node: NodeAddress {
                tree_id: entry.node.tree_id.clone(),
                agent_id: entry.node.agent_id.clone(),
            },
            title: entry.title.clone(),
            workspace: entry.workspace_root.display().to_string(),
            profile: profile_of(&entry),
            updated_at: entry.updated_at_unix,
            residency: SessionResidency::Dormant,
            activity,
            live_work: plakous_host_protocol::wire::LiveWork::default(),
            environment: environment_of(&entry.id),
            session: entry.id,
        }
    }
}

/// Whether the start scan opens a session whose root stands so: a planned
/// handoff's continuation runs by itself, a decided continuation a crash
/// left pending is resubmitted at the open, and a held loss continues by
/// itself only under `resume_after_loss = "continue"`.
const fn owes_open(standing: RootStanding, continue_losses: bool) -> bool {
    match standing {
        RootStanding::Continuing { handoff: true, .. }
        | RootStanding::Continuing { held: false, .. } => true,
        RootStanding::Continuing { held: true, .. } => continue_losses,
        RootStanding::Clear | RootStanding::Stopped => false,
    }
}

impl ProductSessions {
    /// The sessions the host-start scan opens (spec 02 §4.6), among the
    /// directory's rows, newest first.
    ///
    /// # Errors
    ///
    /// The stores cannot be read.
    pub(super) fn owed(&self) -> CliResult<Vec<String>> {
        let continue_losses = crate::product_config::ProductConfig::load(self.services.paths()?)
            .map_err(CliFailure::new)?
            .resume_after_loss
            == Some(crate::product_config::ResumeAfterLoss::Continue);
        let store = self.services.session_records()?;
        let entries = self
            .with_product(|product| product.directory(None))
            .map_err(CliFailure::new)?;
        Ok(entries
            .into_iter()
            .filter(|entry| {
                plakous_hosted::runtime::root_standing(store.as_ref(), &entry.id)
                    .is_ok_and(|standing| owes_open(standing, continue_losses))
            })
            .map(|entry| entry.id)
            .collect())
    }
}

/// What a dormant row's activity is by its root's durable intents.
const fn activity_of(standing: RootStanding) -> SessionActivity {
    match standing {
        RootStanding::Stopped => SessionActivity::Stopped,
        RootStanding::Continuing {
            handoff: false,
            held: true,
        } => SessionActivity::HeldAfterLoss,
        RootStanding::Clear | RootStanding::Continuing { .. } => SessionActivity::Idle,
    }
}

/// The root template, or the binding's account and model when the root
/// runs none.
fn profile_of(entry: &DirectoryEntry) -> Option<String> {
    if let Some(template) = entry
        .root_profile
        .as_ref()
        .and_then(|profile| profile.root_template.clone())
    {
        return Some(template);
    }
    match (&entry.account, &entry.model) {
        (Some(account), Some(model)) => Some(format!("{account}/{model}")),
        (Some(label), None) | (None, Some(label)) => Some(label.clone()),
        (None, None) => None,
    }
}

/// A session `plak host` created has a name-based (v5) id drawn from its
/// request; one a TUI or `plak agent` created a random (v4) one.
fn environment_of(session: &str) -> SessionEnvironment {
    match uuid::Uuid::parse_str(session).map(|id| id.get_version()) {
        Ok(Some(uuid::Version::Sha1)) => SessionEnvironment::HostLoginShell,
        _ => SessionEnvironment::Cli,
    }
}

impl HostSessions<PlakExtension> for ProductSessions {
    fn rows(&self, workspace: Option<&str>) -> Result<Vec<SessionRow>, String> {
        let entries = self.with_product(|store| store.directory(workspace.map(Path::new)))?;
        Ok(entries.into_iter().map(|entry| self.row(entry)).collect())
    }

    fn session_of(&self, node: &NodeAddress) -> Result<Option<String>, String> {
        self.with_product(|store| store.newest_of_tree(&node.tree_id))
    }

    fn open(&self, session: &str) -> CliResult<OpenedSession<PlakExtension>> {
        open_session(
            &self.services,
            session,
            self.idle_release.unwrap_or(IDLE_RELEASE),
        )
    }

    fn refusal(&self, session: &str, failure: &CliFailure) -> RequestRefusal {
        let held = (failure.code() == Some("tree_hosted_elsewhere"))
            .then(|| {
                let tree = self
                    .with_product(|store| store.load(session))
                    .ok()?
                    .node
                    .tree_id;
                self.elsewhere(&tree)
            })
            .flatten();
        match held {
            Some(holder) => RequestRefusal::TreeHostedElsewhere {
                host_name: holder.host_name,
                binary: holder.binary,
                pid: holder.pid,
            },
            None => RequestRefusal::SessionUnavailable {
                detail: failure.message().to_owned(),
            },
        }
    }

    fn create(
        &self,
        workspace: &str,
        template: Option<&str>,
        request_id: &str,
    ) -> Result<Created, RequestRefusal> {
        let request = uuid::Uuid::parse_str(request_id).map_err(|_| RequestRefusal::Malformed {
            detail: "new_session's request_id is not a uuid".to_owned(),
        })?;
        let session = session_for_request(
            &plakous_recursive_subagents::tree::custody_host().host_id,
            request,
        );
        let created = |session: String| Created {
            node: NodeAddress {
                tree_id: session.clone(),
                agent_id: session.clone(),
            },
            session,
            environment: SessionEnvironment::HostLoginShell,
        };
        let unavailable = |failure: CliFailure| RequestRefusal::SessionUnavailable {
            detail: failure.message().to_owned(),
        };
        let store = self.services.session_records().map_err(unavailable)?;
        // A replay of the same request finds the session it created.
        if store.load(&session).is_ok() {
            return Ok(created(session));
        }
        let mut record =
            new_record(&self.services, workspace, template, &session).map_err(unavailable)?;
        match store.save(&mut record) {
            Ok(()) => Ok(created(session)),
            // A concurrent replay created it first.
            Err(_) if store.load(&session).is_ok() => Ok(created(session)),
            Err(error) => Err(RequestRefusal::SessionUnavailable {
                detail: error.to_string(),
            }),
        }
    }
}

/// The session a `new_session` request creates: a name-based uuid of the
/// request in this host's namespace (its `host-id`, itself a uuid), so a
/// retry of the request names the same session. SHA-1 as RFC 9562's v5.
fn session_for_request(host_id: &str, request: uuid::Uuid) -> String {
    let namespace = uuid::Uuid::parse_str(host_id).unwrap_or(uuid::Uuid::nil());
    name_based(namespace, request.as_bytes())
        .hyphenated()
        .to_string()
}

/// RFC 9562's version 5 uuid of `name` in `namespace`.
fn name_based(namespace: uuid::Uuid, name: &[u8]) -> uuid::Uuid {
    let mut digest = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    digest.update(namespace.as_bytes());
    digest.update(name);
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.finish().as_ref()[..16]);
    uuid::Builder::from_sha1_bytes(bytes).into_uuid()
}

/// The record `new_session` saves for `session`: a new session in
/// `workspace` on `template`'s binding, or the workspace's `[root] agent`
/// when none is named, as `plak agent` with no overrides seeds one.
fn new_record(
    services: &Services,
    workspace: &str,
    template: Option<&str>,
    session: &str,
) -> CliResult<crate::store::sessions::SessionRecord> {
    let workspace = std::fs::canonicalize(workspace)
        .ok()
        .filter(|path| path.is_dir())
        .ok_or_else(|| CliFailure::new(format!("{workspace} is not a directory")))?;
    let context = services.paths()?.for_workspace(&workspace);
    let mut config =
        crate::commands::support::admit_product_config(&context, &mut std::io::sink())?;
    if let Some(template) = template {
        if !config.templates.contains_key(template) {
            return Err(CliFailure::new(format!(
                "no template {template} in {}",
                workspace.display()
            )));
        }
        config.root_agent = Some(template.to_owned());
    }
    let mut record = crate::commands::agent::new_agent_record(
        &config, workspace, None, None, None, None, None, false, None,
    )?;
    crate::build_features::admit_selected(
        &config,
        record.account.as_deref(),
        record.model.as_deref(),
        record.composition,
    )?;
    crate::build_features::admit_session(&record)?;
    session.clone_into(&mut record.id);
    record.create_operation_id = format!("plak:create:{session}");
    record.node = plakous_hosted::sessions::SessionNode::genesis(session);
    Ok(record)
}

/// Opens `session` in its own workspace, as `plak agent` opens it with no
/// overrides, on a host worker that releases it once idle.
fn open_session(
    services: &Services,
    session: &str,
    idle_release: std::time::Duration,
) -> CliResult<OpenedSession<PlakExtension>> {
    let record = services
        .session_records()?
        .load(session)
        .map_err(CliFailure::new)?;
    let context = services.paths()?.for_workspace(&record.workspace_root);
    eprintln!("plak host: opening session {session}");
    // A session `new_session` made has no runtime session until its first
    // open creates it; every later open is the exact reopen.
    let opened = if record.plakous_locator.is_none() && record.carry_intent.is_none() {
        crate::commands::agent::AgentSession::Unopened(Box::new(record))
    } else {
        crate::commands::agent::AgentSession::Resume(session.to_owned())
    };
    let mut host = crate::commands::agent::run_agent_after_admission(
        services,
        Some(&context),
        opened,
        None,
        None,
        None,
        None,
        None,
        false,
        None,
    )
    .inspect_err(|failure| {
        eprintln!(
            "plak host: session {session} did not open: {}",
            failure.message()
        );
    })?;
    let attachments = AttachmentCount::default();
    host.idle_release = Some(IdleRelease {
        after: idle_release,
        attachments: attachments.clone(),
    });
    let root = plakous_hosted::host_service::wire::session_root(&host);
    let session_id = host.session.id.clone();
    let state_roots = host.state_roots.clone();
    let (actions, events, worker) = spawn_host_worker(host)?;
    eprintln!("plak host: opened session {session}");
    Ok(OpenedSession {
        root,
        session_id,
        actions,
        events,
        worker,
        attachments,
        roots: Box::new(move |id| session_root_of(&state_roots, id)),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    /// One request names one session, in the shape every store admits, and
    /// a host-created one; another request, or another host, names another.
    #[test]
    fn a_request_names_one_session_per_host() {
        let host = "6f1c6b0e-3d2a-4c1e-9b7a-0d6e2f4a8c11";
        let request = uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap();
        let session = session_for_request(host, request);
        assert_eq!(session, session_for_request(host, request));
        assert_eq!(
            plakous_hosted::sessions::canonical_product_session_id(&session),
            Ok(session.as_str())
        );
        assert_eq!(environment_of(&session), SessionEnvironment::HostLoginShell);
        let other = uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000002").unwrap();
        assert_ne!(session, session_for_request(host, other));
        assert_ne!(
            session,
            session_for_request("0e2a6b1c-1111-4c1e-9b7a-0d6e2f4a8c11", request)
        );
        // The DNS namespace's published vector.
        let dns = uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();
        assert_eq!(
            name_based(dns, b"python.org").to_string(),
            "886313e1-3b8a-5372-9b90-0c9aee199e5d"
        );
    }

    #[test]
    fn a_host_created_session_reads_host_login_shell_and_any_other_cli() {
        let host = uuid::Builder::from_sha1_bytes([7; 16])
            .into_uuid()
            .to_string();
        assert_eq!(environment_of(&host), SessionEnvironment::HostLoginShell);
        let cli = uuid::Builder::from_random_bytes([7; 16])
            .into_uuid()
            .to_string();
        assert_eq!(environment_of(&cli), SessionEnvironment::Cli);
        assert_eq!(environment_of("not-a-uuid"), SessionEnvironment::Cli);
    }

    /// The scan opens what continues by itself, and a held loss only under
    /// `continue`; never a stopped or a clear root.
    #[test]
    fn the_start_scan_opens_what_continues_by_itself() {
        for continue_losses in [false, true] {
            for (standing, owed) in [
                (RootStanding::Clear, false),
                (RootStanding::Stopped, false),
                (
                    RootStanding::Continuing {
                        handoff: true,
                        held: true,
                    },
                    true,
                ),
                (
                    RootStanding::Continuing {
                        handoff: false,
                        held: false,
                    },
                    true,
                ),
                (
                    RootStanding::Continuing {
                        handoff: false,
                        held: true,
                    },
                    continue_losses,
                ),
            ] {
                assert_eq!(
                    owes_open(standing, continue_losses),
                    owed,
                    "{standing:?} under continue={continue_losses}"
                );
            }
        }
    }

    #[test]
    fn a_rows_activity_reads_stop_and_a_held_loss_only() {
        assert_eq!(activity_of(RootStanding::Clear), SessionActivity::Idle);
        assert_eq!(activity_of(RootStanding::Stopped), SessionActivity::Stopped);
        assert_eq!(
            activity_of(RootStanding::Continuing {
                handoff: false,
                held: true
            }),
            SessionActivity::HeldAfterLoss
        );
        // A handoff continues by itself, and a decided loss is pending.
        for (handoff, held) in [(true, true), (false, false), (true, false)] {
            assert_eq!(
                activity_of(RootStanding::Continuing { handoff, held }),
                SessionActivity::Idle
            );
        }
    }

    #[test]
    fn a_rows_profile_is_its_template_else_its_binding() {
        let mut entry = DirectoryEntry {
            id: "s".into(),
            workspace_root: "/w".into(),
            updated_at_unix: 0,
            title: None,
            account: Some("codex".into()),
            model: Some("gpt".into()),
            node: plakous_hosted::sessions::SessionNode::genesis("s"),
            root_profile: None,
        };
        assert_eq!(profile_of(&entry).as_deref(), Some("codex/gpt"));
        entry.root_profile = Some(plakous_hosted::sessions::RootProfile {
            root_template: Some("coder".into()),
            working_set: Vec::new(),
        });
        assert_eq!(profile_of(&entry).as_deref(), Some("coder"));
        entry.root_profile = None;
        entry.account = None;
        entry.model = None;
        assert_eq!(profile_of(&entry), None);
    }
}
