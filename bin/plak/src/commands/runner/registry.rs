//! Home-scoped pairing pins and shared runner placement preparation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use plakous::runner::{
    ConfiguredRunner, MachineOverlapGuard, PlacementFailure, RunnerPin, WorkspaceCatalog,
};
use plakous_hosted::error::{CliFailure, CliResult};

use crate::shell::services::Services;

#[derive(Clone)]
pub(crate) struct Remote {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) config: crate::product_config::RunnerConfig,
    pub(crate) pin: super::PairedRunner,
}

pub(crate) struct Prepared {
    pub(crate) services: Services,
    pub(crate) remote: Vec<Remote>,
    pub(crate) local: Arc<WorkspaceCatalog>,
}

pub(crate) async fn dial(
    services: &Services,
    remote: &Remote,
) -> Result<super::dial::Dialed, PlacementFailure> {
    super::dial::dial_placed(
        services
            .paths()
            .map_err(|_| PlacementFailure::RunnerNotPaired)?,
        &remote.id,
        &remote.config.ssh,
        &remote.config.command,
        false,
    )
    .await
}

/// Restores offline pin evidence before any bind or local workspace admission.
pub(crate) fn prepare(services: &Services) -> CliResult<Prepared> {
    let paths = services.paths()?;
    let config = crate::product_config::ProductConfig::load(paths).map_err(CliFailure::new)?;
    let mut product = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let mut guarded = BTreeSet::new();
    let mut recents = Vec::new();
    for id in product.session_ids().map_err(CliFailure::new)? {
        let record = product.load(&id).map_err(CliFailure::new)?;
        if record
            .runner
            .as_ref()
            .is_none_or(|runner| runner.runner_id == "local")
        {
            guarded.insert(record.workspace_root.clone());
            recents.push((record.updated_at_unix, record.workspace_root));
        }
        if let Some(tree) =
            crate::store::tree::load_tree(paths, &record.node.tree_id).map_err(CliFailure::new)?
        {
            for workspace in tree.workspaces().values() {
                if tree.node(&workspace.owner_agent_id).is_ok_and(|node| {
                    node.placement
                        .runner_id
                        .as_deref()
                        .is_none_or(|id| id == "local")
                }) {
                    guarded.insert(workspace.path.clone());
                }
            }
        }
    }
    recents.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    let mut seen = BTreeSet::new();
    let recents = recents
        .into_iter()
        .filter_map(|(_, path)| seen.insert(path.clone()).then_some(path))
        .take(20)
        .collect::<Vec<_>>();
    let local = WorkspaceCatalog::local(
        paths.workspace().map_err(CliFailure::new)?,
        &recents,
        paths.home(),
    )
    .map_err(|error| CliFailure::new(format!("local workspace options: {error:?}")))?;
    let services = services.with_runner_overlap(MachineOverlapGuard::new(
        plakous::runner::machine_id().ok(),
        guarded,
    ));
    services.runners().configure(ConfiguredRunner {
        runner_id: "local".into(),
        names: vec!["local".into()],
    });
    let mut remote = BTreeMap::<String, Remote>::new();
    let mut names = BTreeMap::<String, Vec<String>>::new();
    for (name, config) in config.runners {
        let path = paths.home().join("runners").join(&name).join("runner.json");
        let pin = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<super::PairedRunner>(&bytes).ok());
        let Some(pin) = pin.filter(|pin| !pin.machine_id.is_empty() && !pin.runner_id.is_empty())
        else {
            let id = format!("unpaired-{name}");
            services.runners().configure(ConfiguredRunner {
                runner_id: id.clone(),
                names: vec![name],
            });
            services.runners().blocked(
                &id,
                PlacementFailure::RunnerNotPaired,
                services.now_unix_seconds(),
            );
            continue;
        };
        names
            .entry(pin.runner_id.clone())
            .or_default()
            .push(name.clone());
        remote
            .entry(pin.runner_id.clone())
            .or_insert_with(|| Remote {
                id: pin.runner_id.clone(),
                name,
                config,
                pin,
            });
    }
    let mut admitted = Vec::new();
    for (id, runner) in remote {
        services.runners().configure(ConfiguredRunner {
            runner_id: id.clone(),
            names: names.remove(&id).unwrap_or_default(),
        });
        match services.runner_overlap().admit_pin(RunnerPin {
            runner_id: id.clone(),
            machine_id: runner.pin.machine_id.clone(),
            roots: runner.pin.roots.clone(),
        }) {
            Ok(()) => admitted.push(runner),
            Err(failure) => services
                .runners()
                .blocked(&id, failure, services.now_unix_seconds()),
        }
    }
    Ok(Prepared {
        services,
        remote: admitted,
        local: Arc::new(local),
    })
}

/// Reconfirm identity and roots before publishing a bound connection. An
/// older pin cannot acquire authority just because its endpoint answers.
pub(crate) fn confirm(
    services: &Services,
    remote: &Remote,
    hello: &plakous::runner::protocol::Hello,
    options: &plakous::runner::WorkspaceOptions,
) -> Result<(), PlacementFailure> {
    if hello.runner_id != remote.id {
        return Err(PlacementFailure::RunnerIdentityMismatch {
            expected: remote.id.clone(),
            actual: hello.runner_id.clone(),
        });
    }
    if hello.machine_id.as_deref() != Some(&remote.pin.machine_id) {
        return Err(PlacementFailure::RunnerNotPaired);
    }
    let roots = options
        .roots
        .clone()
        .ok_or(PlacementFailure::RunnerNotPaired)?;
    services.runner_overlap().admit_pin(RunnerPin {
        runner_id: remote.id.clone(),
        machine_id: remote.pin.machine_id.clone(),
        roots: roots.clone(),
    })?;
    if roots != remote.pin.roots {
        let mut pin = remote.pin.clone();
        pin.roots = roots;
        let path = services
            .paths()
            .map_err(|_| PlacementFailure::RunnerNotPaired)?
            .home()
            .join("runners")
            .join(&remote.name)
            .join("runner.json");
        let bytes =
            serde_json::to_vec_pretty(&pin).map_err(|_| PlacementFailure::RunnerNotPaired)?;
        super::home::write_private(&path, &bytes).map_err(|_| PlacementFailure::RunnerNotPaired)?;
    }
    Ok(())
}
