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

mod archived;
mod lifecycle;
mod options;
mod placement;
mod runner_views;
use placement::Placement;

/// This home's sessions, as `plak host` serves them.
pub(super) struct ProductSessions {
    runtime: Option<tokio::runtime::Handle>,
    services: Services,
    idle_release: Option<std::time::Duration>,
    product: Mutex<Option<ProductStore>>,
    tree: SqliteTreeStore,
    placement: Box<dyn Placement>,
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
        runtime: Option<tokio::runtime::Handle>,
    ) -> CliResult<Self> {
        let tree =
            crate::store::tree::open_tree_store(services.paths()?).map_err(CliFailure::new)?;
        Ok(Self {
            runtime: runtime.clone(),
            placement: Box::new(placement::RegistryPlacement {
                services: services.clone(),
                runtime: runtime.clone(),
            }),
            services: services.clone(),
            idle_release,
            product: Mutex::new(None),
            tree,
        })
    }

    fn stop_roots(
        &self,
        record: &crate::store::sessions::SessionRecord,
    ) -> CliResult<plakous_hosted::agent::ProductStateRoots> {
        let context = if record.runner.is_some() {
            self.services
                .paths()?
                .for_remote_workspace(&record.workspace_root)
        } else {
            self.services.paths()?.for_workspace(&record.workspace_root)
        };
        Ok(self.services.state_roots_for(&context)?.for_runner_stop(
            self.services.runners().clone(),
            record
                .runner
                .as_ref()
                .map_or("local", |runner| runner.runner_id.as_str()),
        ))
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

    /// The live process other than this one that holds the session of
    /// `tree_id` and `locator`: its tree's custody holder, else its runtime
    /// lease's holder, each unless proven gone.
    ///
    /// # Errors
    ///
    /// The tree store cannot be read: a holder is never read free on an
    /// error.
    fn elsewhere(
        &self,
        tree_id: &str,
        locator: Option<plakous::sessions::SessionLocator>,
    ) -> Result<Option<HostedElsewhere>, String> {
        let custody = holder_elsewhere(&self.tree, tree_id)
            .map_err(|error| format!("the tree store could not be read: {error}"))?;
        if let Some(holder) = custody {
            return Ok(Some(HostedElsewhere {
                host_name: holder.host_name,
                binary: holder.binary,
                pid: holder.instance.pid(),
            }));
        }
        let paths = self
            .services
            .paths()
            .map_err(|failure| failure.message().to_owned())?;
        Ok(locator
            .and_then(|locator| crate::store::holders::runtime_holder(paths, locator))
            .map(|holder| HostedElsewhere {
                host_name: holder.host_name,
                binary: None,
                pid: holder.pid,
            }))
    }

    fn row(&self, entry: DirectoryEntry) -> Result<SessionRow, String> {
        use plakous_recursive_subagents::tree::TreeStore as _;
        let store = self
            .services
            .session_records()
            .map_err(|failure| failure.message().to_owned())?;
        let activity = activity_of(plakous_hosted::runtime::root_standing(
            store.as_ref(),
            &entry.id,
        )?);
        let hosted_elsewhere = self.elsewhere(&entry.node.tree_id, entry.locator)?;
        let record = store.load(&entry.id)?;
        let paths = self
            .services
            .paths()
            .map_err(|failure| failure.message().to_owned())?;
        let context = if record.runner.is_some() {
            paths.for_remote_workspace(&record.workspace_root)
        } else {
            paths.for_workspace(&record.workspace_root)
        };
        let config = crate::product_config::ProductConfig::load(&context);
        let roster = self
            .tree
            .load(&entry.node.tree_id)
            .map_err(|error| error.detail)?
            .map(plakous_recursive_subagents::tree::ProductAgentTree::reopen)
            .map(|tree| tree.current_epoch().clone());
        let profile = config.map(|config| {
            crate::shell::settings::session_profile(
                &config,
                paths.home(),
                &plakous_hosted::runtime::SurfaceSession {
                    runner: record.runner.clone(),
                    id: record.id.clone(),
                    account: record.account.clone(),
                    model: record.model.clone(),
                    effort: record.effort.clone(),
                    fast: record.target_selection
                        == crate::product_config::ProductTargetSelection::NativePriority,
                    workspace: record.workspace_root.clone(),
                    roster,
                    live_children: false,
                    mcp: None,
                },
            )
        });
        let (profile, differences) = match profile {
            Ok(profile) => (Some(profile.id), profile.differences),
            Err(_) => (
                Some("custom".to_owned()),
                vec![plakous_host_protocol::views::ProfileDifference::Roster],
            ),
        };
        let runners = self.services.runners().rows();
        let runner_id = record
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str());
        Ok(SessionRow {
            runner: runner_views::label(runner_id, &runners),
            runner_status: runner_views::status(runner_id, &runners)?,
            needs_operator: false,
            activity_seq: entry
                .locator
                .map(|locator| {
                    plakous::sessions::AgentSessions::inspect_branch_revision_sqlite(
                        paths.runtime_database(),
                        locator,
                    )
                    .map_err(|error| error.to_string())
                })
                .transpose()?
                .unwrap_or(0),
            archived: entry.archived,
            differences,
            environment: environment_of(hosted_elsewhere.is_some()),
            hosted_elsewhere,
            node: NodeAddress {
                tree_id: entry.node.tree_id.clone(),
                agent_id: entry.node.agent_id.clone(),
            },
            title: entry.title.clone(),
            workspace: entry.workspace_root.display().to_string(),
            profile,
            updated_at: entry.updated_at_unix,
            residency: SessionResidency::Dormant,
            activity,
            live_work: plakous_host_protocol::wire::LiveWork::default(),
            session: entry.id,
        })
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
        RootStanding::Continuing { .. } => SessionActivity::Continuing,
        RootStanding::Clear => SessionActivity::Idle,
    }
}

/// A session another process holds runs its tools there; any other is, or
/// would be, served by this host, with its environment.
const fn environment_of(held_elsewhere: bool) -> SessionEnvironment {
    if held_elsewhere {
        SessionEnvironment::Cli
    } else {
        SessionEnvironment::Host
    }
}

impl HostSessions<PlakExtension> for ProductSessions {
    fn archived_view(
        &self,
        session: &str,
    ) -> Result<
        Option<plakous_hosted::host_service::wire::ArchivedSessionView<PlakExtension>>,
        RequestRefusal,
    > {
        self.read_archived(session)
    }

    fn runner_label(&self, id: &str) -> plakous_host_protocol::wire::RunnerLabel {
        runner_views::label(id, &self.services.runners().rows())
    }

    fn options(
        &self,
        workspace: Option<&plakous_host_protocol::wire::WorkspaceProbe>,
        progress: &mut dyn FnMut(
            plakous_hosted::host_service::wire::PickerProgress,
        ) -> Result<(), String>,
    ) -> Result<plakous_host_protocol::wire::SessionOptions, RequestRefusal> {
        self.session_options(workspace, progress)
    }

    fn rename(&self, session: &str, title: Option<&str>) -> Result<Option<String>, RequestRefusal> {
        crate::commands::session::lifecycle::rename_session(&self.services, session, title)
            .map_err(|error| lifecycle::refusal(session, error))
    }

    fn archive(&self, session: &str, archived: bool) -> Result<(), RequestRefusal> {
        crate::commands::session::lifecycle::set_archived(&self.services, session, archived)
            .map_err(|error| lifecycle::refusal(session, error))
    }

    fn delete(&self, session: &str, abandon_children: bool) -> Result<(), RequestRefusal> {
        crate::commands::session::lifecycle::delete_confirmed(
            &self.services,
            session,
            abandon_children,
        )
        .map_err(|error| lifecycle::refusal(session, error))
    }

    fn rows(
        &self,
        workspace: Option<&str>,
        runner: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<SessionRow>, String> {
        let entries = self.with_product(|store| {
            store.directory_filtered(workspace.map(Path::new), runner, include_archived)
        })?;
        entries.into_iter().map(|entry| self.row(entry)).collect()
    }

    fn session_of(&self, node: &NodeAddress) -> Result<Option<String>, String> {
        self.with_product(|store| store.newest_of_tree(&node.tree_id))
    }

    fn open(&self, session: &str) -> CliResult<OpenedSession<PlakExtension>> {
        let record = self
            .services
            .session_records()?
            .load(session)
            .map_err(CliFailure::new)?;
        let runner_id = record
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str());
        self.placement
            .validate(
                &runner_views::label(runner_id, &self.services.runners().rows()),
                record
                    .workspace_root
                    .to_str()
                    .ok_or_else(|| CliFailure::new("runner workspace is not UTF-8"))?,
            )
            .map_err(|refusal| {
                CliFailure::new("session placement refused before open").with_wire_refusal(refusal)
            })?;
        open_session(
            &self.services,
            session,
            self.idle_release.unwrap_or(IDLE_RELEASE),
        )
    }

    fn rebind(&self, session: &str, cause: &str) -> CliResult<Created> {
        let record = self
            .services
            .session_records()?
            .load(session)
            .map_err(CliFailure::new)?;
        let (target, _) =
            crate::carry::rebind::session::rebind_session(&self.services, &record, cause)?;
        Ok(Created {
            node: NodeAddress {
                tree_id: target.node.tree_id,
                agent_id: target.node.agent_id,
            },
            session: target.id,
            environment: SessionEnvironment::Host,
        })
    }

    fn stop_dormant(&self, session: &str) -> CliResult<()> {
        let store = self.services.session_records()?;
        let mut record = store.load(session).map_err(CliFailure::new)?;
        if let Some(holder) = self
            .elsewhere(&record.node.tree_id, record.plakous_locator)
            .map_err(CliFailure::new)?
        {
            return Err(CliFailure::hosted_elsewhere(format!(
                "another live process holds this session (pid {})",
                holder.pid
            )));
        }
        let roots = self.stop_roots(&record)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| CliFailure::new(error.to_string()))?;
        runtime.block_on(plakous_hosted::runtime::stop_dormant_session(
            &roots,
            store.as_ref(),
            &mut record,
        ))
    }

    fn runner_pending(&self, session: &str) -> bool {
        let roots = (|| -> CliResult<_> {
            let record = self
                .services
                .session_records()?
                .load(session)
                .map_err(CliFailure::new)?;
            self.stop_roots(&record)
        })();
        roots.map_or(true, |roots| roots.runner_pending())
    }

    fn refusal(&self, session: &str, failure: &CliFailure) -> RequestRefusal {
        if let Some(refusal) = failure.wire_refusal() {
            return refusal.clone();
        }
        let held = (failure.code() == Some("tree_hosted_elsewhere"))
            .then(|| {
                let record = self.with_product(|store| store.load(session)).ok()?;
                self.elsewhere(&record.node.tree_id, record.plakous_locator)
                    .ok()
                    .flatten()
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
        creation: &plakous_hosted::host_service::wire::Creation<'_>,
    ) -> Result<Created, RequestRefusal> {
        if creation.profile.is_some() && (creation.template.is_some() || creation.model.is_some()) {
            return Err(RequestRefusal::Malformed {
                detail: "profile cannot be combined with template or model".to_owned(),
            });
        }
        let workspace = creation.workspace;
        let request_id = creation.request_id;
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
            environment: SessionEnvironment::Host,
        };
        let unavailable = |failure: CliFailure| RequestRefusal::SessionUnavailable {
            detail: failure.message().to_owned(),
        };
        let store = self.services.session_records().map_err(unavailable)?;
        let replayed = |existing: &crate::store::sessions::SessionRecord| {
            if replays(existing, creation) {
                Ok(created(session.clone()))
            } else {
                Err(RequestRefusal::Malformed {
                    detail: "new_session's request_id names a session made with another \
                             workspace or template"
                        .to_owned(),
                })
            }
        };
        // A replay of the same request finds the session it created.
        if let Ok(existing) = store.load(&session) {
            return replayed(&existing);
        }
        let validated = self.placement.validate(creation.runner, workspace)?;
        let local_workspace = validated
            .runner
            .is_none()
            .then(|| validated.workspace.clone());
        let mut record = new_record(&self.services, creation, validated, &session)?;
        let mut commit = || {
            #[cfg(test)]
            crate::store::lifecycle_crash_cut("host_create_before_commit");
            match store.save(&mut record) {
                Ok(()) => {
                    #[cfg(test)]
                    crate::store::lifecycle_crash_cut("host_create_after_commit");
                    Ok(created(session.clone()))
                }
                // A concurrent replay created it first.
                Err(error) => match store.load(&session) {
                    Ok(existing) => replayed(&existing),
                    Err(_) => Err(RequestRefusal::SessionUnavailable {
                        detail: error.to_string(),
                    }),
                },
            }
        };
        if let Some(workspace) = local_workspace {
            return self
                .services
                .runner_overlap()
                .with_local_admission(workspace, commit)
                .map_err(|failure| {
                    runner_views::refusal(creation.runner, failure, &self.services.runners().rows())
                })?;
        }
        commit()
    }
}

/// A committed create compares only stored facts, never today's configuration,
/// filesystem or runner liveness. Requested profile ids are not persisted.
fn replays(
    existing: &crate::store::sessions::SessionRecord,
    creation: &plakous_hosted::host_service::wire::Creation<'_>,
) -> bool {
    existing.workspace_root.to_str() == Some(creation.workspace)
        && existing
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str())
            == creation.runner.runner_id
        && creation.template.is_none_or(|template| {
            existing
                .root_profile
                .as_ref()
                .and_then(|profile| profile.root_template.as_deref())
                == Some(template)
        })
        && creation.model.is_none_or(|model| {
            existing.account.as_deref() == Some(model.account.as_str())
                && existing.model.as_deref() == Some(model.model.as_str())
        })
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
    creation: &plakous_hosted::host_service::wire::Creation<'_>,
    validated: placement::Validated,
    session: &str,
) -> Result<crate::store::sessions::SessionRecord, RequestRefusal> {
    use plakous_host_protocol::wire::BindingRefusalKind;
    let binding = |kind| RequestRefusal::BindingRefused { kind };
    let workspace = validated.workspace;
    let paths = services
        .paths()
        .map_err(|failure| RequestRefusal::SessionUnavailable {
            detail: failure.message().to_owned(),
        })?;
    let context = if validated.runner.is_some() {
        crate::home::paths::PlakPaths::explicit(paths.home().to_owned())
    } else {
        paths.for_workspace(&workspace)
    };
    let mut config = crate::commands::support::admit_product_config(&context, &mut std::io::sink())
        .map_err(|_| binding(BindingRefusalKind::Configuration))?;
    if let Some(profile) = creation.profile {
        config = crate::profiles::prepare(&config, context.home(), profile)
            .map_err(|_| binding(BindingRefusalKind::UnknownProfile))?
            .config;
    }
    if let Some(template) = creation.template {
        if !config.templates.contains_key(template) {
            return Err(binding(BindingRefusalKind::UnknownTemplate));
        }
        config.root_agent = Some(template.to_owned());
    }
    if let Some(model) = creation.model
        && let Some(template) = config
            .root_agent
            .as_ref()
            .and_then(|name| config.templates.get_mut(name))
    {
        template.account.clone_from(&model.account);
        template.model.clone_from(&model.model);
    }
    let account: Option<plakous_hosted::account::AccountRef> = creation
        .model
        .map(|model| model.account.parse())
        .transpose()
        .map_err(|_| binding(BindingRefusalKind::ModelUnavailable))?;
    let selected_model = creation.model.map(|model| model.model.clone());
    let platform = account.as_ref().and_then(|account| {
        plakous_hosted::binding::PlatformProvider::for_account(account.as_str())
    });
    let wire = account
        .as_ref()
        .filter(|account| account.vertical() == plakous_hosted::account::Vertical::OpenRouter)
        .map(|_| crate::product_config::ProductWireBinding::resolve(config.default_protocol()));
    let mut record = crate::commands::agent::new_agent_record(
        &config,
        workspace,
        account,
        None,
        platform,
        wire,
        selected_model,
        None,
        false,
        None,
        Some(validated.probe.as_ref()),
    )
    .map_err(|_| binding(BindingRefusalKind::ModelUnavailable))?;
    record.runner = validated.runner;
    record.root_profile = Some(plakous_hosted::sessions::RootProfile {
        root_template: config.root_agent.clone(),
        working_set: config
            .working_set
            .names()
            .iter()
            .map(|name| name.as_str().to_owned())
            .collect(),
    });
    crate::build_features::admit_selected(
        &config,
        record.account.as_deref(),
        record.model.as_deref(),
        record.composition,
    )
    .map_err(|_| binding(BindingRefusalKind::IncompatibleComposition))?;
    crate::build_features::admit_session(&record)
        .map_err(|_| binding(BindingRefusalKind::IncompatibleComposition))?;
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
    let context = if record.runner.is_some() {
        services
            .paths()?
            .for_remote_workspace(&record.workspace_root)
    } else {
        services.paths()?.for_workspace(&record.workspace_root)
    };
    eprintln!("plak host: opening session {session}");
    // A session `new_session` made has no runtime session until its first
    // open, by this host or any face, creates it.
    let mut host = crate::commands::agent::run_agent_after_admission(
        services,
        Some(&context),
        crate::commands::agent::AgentSession::Resume(session.to_owned()),
        None,
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
    let pending_roots = state_roots.clone();
    let (actions, events, worker) = spawn_host_worker(host)?;
    eprintln!("plak host: opened session {session}");
    Ok(OpenedSession {
        runner_pending: Some(std::sync::Arc::new(move || pending_roots.runner_pending())),
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

    #[test]
    fn opening_local_before_daemon_readiness_never_falls_back_to_direct_execution() {
        let (temporary, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let paths = paths.for_workspace(&workspace);
        let services = Services::for_home(paths.clone());
        services
            .runners()
            .configure(plakous::runner::ConfiguredRunner {
                runner_id: "local".into(),
                names: vec!["local".into()],
            });
        let mut record = crate::store::sessions::SessionRecord::new(workspace);
        record.account = Some("codex".into());
        record.model = Some("gpt-test".into());
        record.root_profile = Some(plakous_hosted::sessions::RootProfile {
            root_template: None,
            working_set: Vec::new(),
        });
        services
            .session_records()
            .unwrap()
            .save(&mut record)
            .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let product =
            ProductSessions::new(&services, None, Some(runtime.handle().clone())).unwrap();
        let Err(failure) = product.open(&record.id) else {
            panic!("an unready local daemon must not admit a direct runtime");
        };
        assert!(
            matches!(
                product.refusal(&record.id, &failure),
                RequestRefusal::PlacementRefused { .. }
            ),
            "{failure}"
        );
        assert!(!paths.runtime_database().exists());
        assert!(
            services
                .session_records()
                .unwrap()
                .load(&record.id)
                .unwrap()
                .plakous_locator
                .is_none()
        );
    }

    #[test]
    fn committed_creation_replay_reads_only_stored_facts_not_aliases_profiles_or_paths() {
        use plakous_host_protocol::wire::{CreationModel, RunnerLabel};
        use plakous_hosted::host_service::wire::Creation;
        let mut saved = crate::store::sessions::SessionRecord::new("/missing/work".into());
        saved.account = Some("claude/work".into());
        saved.model = Some("frozen-model".into());
        saved.root_profile = Some(plakous_hosted::sessions::RootProfile {
            root_template: Some("saved-root".into()),
            working_set: vec!["saved-worker".into()],
        });
        saved.runner = Some(plakous_hosted::sessions::RunnerDisplay {
            name: "old-name".into(),
            runner_id: "saved-id".into(),
            workspace_root: "/missing/work".into(),
            workspace_cwd: "/missing/work".into(),
        });
        let runner = RunnerLabel {
            name: "new-name".into(),
            runner_id: "saved-id".into(),
        };
        let mut replay = Creation {
            runner: &runner,
            workspace: "/missing/work",
            profile: Some("removed-profile"),
            template: None,
            model: None,
            request_id: "unused",
        };
        assert!(replays(&saved, &replay));
        replay.profile = None;
        replay.template = Some("saved-root");
        let model = CreationModel {
            account: "claude/work".into(),
            model: "frozen-model".into(),
        };
        replay.model = Some(&model);
        assert!(replays(&saved, &replay));
        replay.template = Some("replacement-root");
        assert!(!replays(&saved, &replay));
        replay.template = None;
        replay.workspace = "/missing/../work";
        assert!(!replays(&saved, &replay));
        replay.workspace = "/missing/work";
        let replacement = RunnerLabel {
            name: "old-name".into(),
            runner_id: "replacement-id".into(),
        };
        replay.runner = &replacement;
        assert!(!replays(&saved, &replay));
        replay.runner = &runner;
        let changed_model = CreationModel {
            account: "claude/work".into(),
            model: "other-model".into(),
        };
        replay.model = Some(&changed_model);
        assert!(!replays(&saved, &replay));
    }

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

    /// A session another process holds runs its tools there; any other is
    /// this host's.
    #[test]
    fn a_rows_environment_is_its_holders() {
        assert_eq!(environment_of(false), SessionEnvironment::Host);
        assert_eq!(environment_of(true), SessionEnvironment::Cli);
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
                SessionActivity::Continuing
            );
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "crash contract tests"
)]
mod crash_tests;
