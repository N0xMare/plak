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
    pub(crate) config: crate::product_config::RunnerConfig,
    pub(crate) pin: super::PairedRunner,
}

pub(crate) struct Prepared {
    pub(crate) services: Services,
    pub(crate) local: Arc<WorkspaceCatalog>,
}

fn configured_runners(paths: &crate::home::paths::PlakPaths) -> CliResult<Vec<ConfiguredRunner>> {
    let config = crate::product_config::ProductConfig::load(paths).map_err(CliFailure::new)?;
    let mut aliases = BTreeMap::<String, Vec<String>>::new();
    aliases.insert("local".into(), vec!["local".into()]);
    for name in config.runners.keys() {
        let id =
            read_pin(paths, name).map_or_else(|| format!("unpaired-{name}"), |pin| pin.runner_id);
        aliases.entry(id).or_default().push(name.clone());
    }
    Ok(aliases
        .into_iter()
        .map(|(runner_id, names)| ConfiguredRunner { runner_id, names })
        .collect())
}

fn read_pin(paths: &crate::home::paths::PlakPaths, name: &str) -> Option<super::PairedRunner> {
    let path = paths.home().join("runners").join(name).join("runner.json");
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<super::PairedRunner>(&bytes).ok())
        .filter(|pin| !pin.machine_id.is_empty() && !pin.runner_id.is_empty())
}

pub(crate) fn remote(services: &Services, id: &str) -> Result<Remote, PlacementFailure> {
    let paths = services
        .paths()
        .map_err(|_| PlacementFailure::RunnerNotPaired)?;
    let config = crate::product_config::ProductConfig::load(paths)
        .map_err(|_| PlacementFailure::RunnerNotPaired)?;
    for (name, config) in config.runners {
        if let Some(pin) = read_pin(paths, &name)
            && pin.runner_id == id
        {
            services.runner_overlap().admit_pin(RunnerPin {
                runner_id: id.into(),
                machine_id: pin.machine_id.clone(),
                roots: pin.roots.clone(),
            })?;
            return Ok(Remote {
                id: id.into(),
                config,
                pin,
            });
        }
    }
    if id.starts_with("unpaired-") {
        Err(PlacementFailure::RunnerNotPaired)
    } else {
        Err(PlacementFailure::RunnerNotConfigured {
            saved_id: id.into(),
        })
    }
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

/// The shared lock covers the durable snapshot read and admission commit,
/// including standalone pairing while the host admits a local workspace.
pub(crate) fn overlap_guard(
    paths: &crate::home::paths::PlakPaths,
) -> CliResult<MachineOverlapGuard> {
    let directory = paths.home().join("runners");
    super::private_directory(&directory)?;
    let saved = paths.clone();
    Ok(
        MachineOverlapGuard::new(plakous::runner::machine_id().ok(), []).with_admission_store(
            directory.join("admission.lock"),
            move || {
                admission_snapshot(&saved).map_err(|_| PlacementFailure::RunnerUnreachable {
                    phase: plakous::runner::ProbePhase::Reconcile,
                    timed_out: false,
                })
            },
        ),
    )
}

fn admission_snapshot(
    paths: &crate::home::paths::PlakPaths,
) -> CliResult<plakous::runner::AdmissionSnapshot> {
    let mut product = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let mut local = BTreeSet::new();
    for id in product.session_ids().map_err(CliFailure::new)? {
        let record = product.load(&id).map_err(CliFailure::new)?;
        if record
            .runner
            .as_ref()
            .is_none_or(|runner| runner.runner_id == "local")
        {
            local.insert(record.workspace_root);
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
                    local.insert(workspace.path.clone());
                }
            }
        }
    }
    let config = crate::product_config::ProductConfig::load(paths).map_err(CliFailure::new)?;
    let mut pins = BTreeMap::<String, RunnerPin>::new();
    for name in config.runners.keys() {
        let path = paths.home().join("runners").join(name).join("runner.json");
        if let Some(pin) = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<super::PairedRunner>(&bytes).ok())
            .filter(|pin| !pin.machine_id.is_empty() && !pin.runner_id.is_empty())
        {
            let saved = pins
                .entry(pin.runner_id.clone())
                .or_insert_with(|| RunnerPin {
                    runner_id: pin.runner_id.clone(),
                    machine_id: pin.machine_id.clone(),
                    roots: Vec::new(),
                });
            if saved.machine_id != pin.machine_id {
                return Err(CliFailure::new(
                    "runner aliases disagree on machine identity; re-pair the aliases",
                ));
            }
            saved.roots.extend(pin.roots);
            saved.roots.sort();
            saved.roots.dedup();
        }
    }
    Ok(plakous::runner::AdmissionSnapshot {
        local,
        pins: pins.into_values().collect(),
    })
}

/// Restores offline pin evidence before any bind or local workspace admission.
pub(crate) fn prepare(services: &Services) -> CliResult<Prepared> {
    let paths = services.paths()?;
    let mut product = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let mut recents = Vec::new();
    let mut required = BTreeSet::new();
    for id in product.session_ids().map_err(CliFailure::new)? {
        let record = product.load(&id).map_err(CliFailure::new)?;
        let runner_id = record
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str())
            .to_owned();
        if product
            .load_intents(&id)
            .map_err(CliFailure::new)?
            .root_interrupts
            .iter()
            .any(|intent| {
                intent.value.cause
                    == plakous_hosted::sessions::projector::RootInterruptCause::Stopped
            })
        {
            required.insert(runner_id.clone());
        }
        if record
            .runner
            .as_ref()
            .is_none_or(|runner| runner.runner_id == "local")
        {
            recents.push((record.updated_at_unix, record.workspace_root));
        }
        if let Some(tree) =
            crate::store::tree::load_tree(paths, &record.node.tree_id).map_err(CliFailure::new)?
        {
            if !tree.recorded_process_stops().is_empty() {
                required.insert(runner_id);
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
    let empty = WorkspaceCatalog::local(None, &[], paths.home()).map_err(|_| {
        CliFailure::new("the product home is unavailable for local workspace discovery")
    })?;
    tracing::info!(target: "plak.runner", "local workspace catalog ignores the process launch directory");
    let recents = recents.into_iter().filter(|path| {
        let admitted = path.to_str().is_some_and(|path| empty.validate(path, true).is_ok());
        if !admitted {
            tracing::warn!(target: "plak.runner", "inadmissible recent workspace omitted from local catalog");
        }
        admitted
    }).collect::<Vec<_>>();
    let local = WorkspaceCatalog::local(None, &recents, paths.home()).map_err(|_| {
        CliFailure::new("the product home is unavailable for local workspace discovery")
    })?;
    let services = services.with_runner_overlap(overlap_guard(paths)?);
    for runner in configured_runners(paths)? {
        services.runners().configure(runner);
    }
    let configured_paths = paths.clone();
    services.runners().configured_with(Arc::new(move || {
        configured_runners(&configured_paths).map_err(|_| PlacementFailure::RunnerUnreachable {
            phase: plakous::runner::ProbePhase::Options,
            timed_out: false,
        })
    }));
    for id in required {
        services.runners().require_connection(&id, true);
    }
    Ok(Prepared {
        services,
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
    services.runner_overlap().with_pin_admission(
        RunnerPin {
            runner_id: remote.id.clone(),
            machine_id: remote.pin.machine_id.clone(),
            roots: roots.clone(),
        },
        || {
            let paths = services
                .paths()
                .map_err(|_| PlacementFailure::RunnerNotPaired)?;
            let config = crate::product_config::ProductConfig::load(paths)
                .map_err(|_| PlacementFailure::RunnerNotPaired)?;
            for name in config.runners.keys() {
                let Some(mut pin) = read_pin(paths, name).filter(|pin| pin.runner_id == remote.id)
                else {
                    continue;
                };
                if pin.roots == roots {
                    continue;
                }
                pin.roots.clone_from(&roots);
                let path = paths.home().join("runners").join(name).join("runner.json");
                let bytes = serde_json::to_vec_pretty(&pin)
                    .map_err(|_| PlacementFailure::RunnerNotPaired)?;
                super::home::write_private(&path, &bytes)
                    .map_err(|_| PlacementFailure::RunnerNotPaired)?;
            }
            Ok(())
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "isolated alias pin fixtures")]
mod tests {
    use super::*;

    #[test]
    fn confirmation_repairs_every_alias_even_when_the_selected_pin_is_current() {
        use plakous::runner::protocol::VersionTriple;
        use plakous::runner::{RunnerIdentity, RunnerOptions, RunnerService};
        use plakous::tools::{
            ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
        };
        let (temporary, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        let roots = ["old", "new", "other"].map(|name| {
            let path = temporary.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        });
        std::fs::write(paths.home().join("config.toml"), "[runners.alpha]\nssh='fixture'\ncommand='/bin/false'\n[runners.beta]\nssh='fixture'\ncommand='/bin/false'\n[runners.gamma]\nssh='fixture'\ncommand='/bin/false'\n").unwrap();
        let machine = "fixture-machine".to_owned();
        for (name, id, root) in [
            ("alpha", "runner", &roots[1]),
            ("beta", "runner", &roots[0]),
            ("gamma", "other", &roots[2]),
        ] {
            let directory = paths.home().join("runners").join(name);
            super::super::private_directory(&directory).unwrap();
            let pin = super::super::PairedRunner {
                runner_id: id.into(),
                source_revision: "review".into(),
                flavor: "test".into(),
                binary_id: None,
                machine_id: machine.clone(),
                roots: vec![root.to_str().unwrap().into()],
            };
            super::super::home::write_private(
                &directory.join("runner.json"),
                &serde_json::to_vec(&pin).unwrap(),
            )
            .unwrap();
        }
        let services = Services::for_home(paths.clone())
            .with_runner_overlap(MachineOverlapGuard::new(Some(machine.clone()), []));
        let config = crate::product_config::ProductConfig::load(&paths).unwrap();
        let remote = Remote {
            id: "runner".into(),
            config: config.runners["alpha"].clone(),
            pin: read_pin(&paths, "alpha").unwrap(),
        };
        let catalog =
            WorkspaceCatalog::machine(vec![roots[1].clone()], paths.home(), false).unwrap();
        let service = RunnerService::with_options(
            ExecutionEnvironment::host(
                ExecutionEnvironmentScope::Run(
                    "00000000-0000-4000-8000-000000000001".parse().unwrap(),
                ),
                &roots[1],
                ".",
                ExecutionEnvironmentConfig::default(),
            )
            .unwrap(),
            RunnerIdentity {
                runner_id: RunnerId::new("runner").unwrap(),
                version: VersionTriple::current("review", "test"),
                boot_id: "boot".into(),
                binary_id: None,
            },
            RunnerOptions {
                machine_id: Some(machine),
                ..RunnerOptions::default()
            },
        )
        .unwrap();
        confirm(&services, &remote, service.hello(), &catalog.options()).unwrap();
        for name in ["alpha", "beta"] {
            assert_eq!(
                read_pin(&paths, name).unwrap().roots,
                vec![roots[1].to_str().unwrap()]
            );
        }
        assert_eq!(
            read_pin(&paths, "gamma").unwrap().roots,
            vec![roots[2].to_str().unwrap()]
        );
    }
}
