//! `plak doctor`: gathers the product, state, credential and sidecar checks into one report.

use std::env;
use std::io;
use std::path::PathBuf;

use plakous::subscriptions::NativeProvider;

use crate::commands::application::{AccountOps as _, PlakousApplication};
use crate::presentation::{cli_human_output, title_case_token};
use crate::shell::services::Services;
use crate::{
    CliFailure, CliFailureKind, CliResult, ProcessOutcome, cli_output, home, openrouter_product,
    platform_catalog, presentation, product_config, product_doctor, roster_input, sidecars, store,
    telemetry_sink, telemetry_store,
};

pub(crate) fn run_doctor(
    services: &Services,
    stdout: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    let paths = services.paths()?;
    let doctor_paths = product_doctor::DoctorPaths::under(paths.home().to_path_buf());
    let loaded = product_config::ProductConfig::load(paths);
    let config = product_doctor::inspect_config(&paths.config_file(), |_| {
        loaded.as_ref().map(|_| ()).map_err(Clone::clone)
    });
    let config = match (&loaded, config) {
        (Err(error), product_doctor::ConfigStatus::Missing) => {
            product_doctor::ConfigStatus::Invalid(error.clone())
        }
        (_, config) => config,
    };
    let selected = loaded.unwrap_or_else(|_| product_config::ProductConfig::default_product());
    let product_database =
        product_doctor::inspect_state_database(&paths.product_database(), store::SCHEMA_VERSION);
    let subagents_database = product_doctor::inspect_state_database(
        &paths.subagents_database(),
        plakous_recursive_subagents_store::SCHEMA_VERSION,
    );
    let runtime_database = product_doctor::inspect_runtime_database(&paths.runtime_database());
    let runtime_store = product_doctor::inspect_runtime_store(&paths.runtime_database());
    let selected_composition = platform_catalog::root_composition(&selected);
    let telemetry = telemetry_sink::inspect_product_telemetry(paths);
    let local_composition_required = matches!(
        selected_composition,
        product_config::ProductCompositionSelection::JsPtc
            | product_config::ProductCompositionSelection::PyPtc
            | product_config::ProductCompositionSelection::NativePtc
    ) || (selected_composition
        == product_config::ProductCompositionSelection::FollowCatalog
        && selected.default_account() == Some("codex"));
    let composition_host = match selected_composition {
        product_config::ProductCompositionSelection::PyPtc => {
            match sidecars::python_bundle::locate() {
                Ok(path) => product_doctor::CompositionHostStatus::Included {
                    identity: path.display().to_string(),
                },
                Err(error) => product_doctor::CompositionHostStatus::Unavailable(error),
            }
        }
        product_config::ProductCompositionSelection::JsPtc
        | product_config::ProductCompositionSelection::NativePtc => {
            match sidecars::js_host::locate() {
                Ok(path) => product_doctor::CompositionHostStatus::Included {
                    identity: path.display().to_string(),
                },
                Err(error) => product_doctor::CompositionHostStatus::Unavailable(error),
            }
        }
        product_config::ProductCompositionSelection::FollowCatalog
            if selected.default_account() == Some("codex") =>
        {
            match sidecars::js_host::locate() {
                Ok(path) => product_doctor::CompositionHostStatus::Included {
                    identity: path.display().to_string(),
                },
                Err(error) => product_doctor::CompositionHostStatus::Unavailable(error),
            }
        }
        _ => product_doctor::CompositionHostStatus::Included {
            identity: "not required by the selected composition".to_owned(),
        },
    };
    let openrouter_catalog = match openrouter_product::probe_openrouter_catalog_allowlist(
        paths,
        selected.default_account(),
        selected.default_model(),
    ) {
        openrouter_product::OpenRouterCatalogProbe::NotConfigured => {
            product_doctor::OpenRouterCatalogStatus::NotConfigured
        }
        openrouter_product::OpenRouterCatalogProbe::KeyPresentNoModel => {
            product_doctor::OpenRouterCatalogStatus::KeyPresentNoModel
        }
        openrouter_product::OpenRouterCatalogProbe::Ok => {
            product_doctor::OpenRouterCatalogStatus::Ok
        }
        openrouter_product::OpenRouterCatalogProbe::Failed(detail) => {
            product_doctor::OpenRouterCatalogStatus::Failed(detail)
        }
    };
    // `config OK` means the effective config and all template files parsed. Say before launch
    // whether the template new sessions bind is actually attachable. Both
    // inputs are local — the persisted catalog and the offline credential
    // posture — so this adds no network I/O to `doctor`.
    let root_template = match selected.root_agent.as_deref() {
        None => product_doctor::RootTemplateStatus::Unset,
        Some(name) => match selected.templates.get(name) {
            None => product_doctor::RootTemplateStatus::Undefined {
                name: name.to_owned(),
            },
            Some(template) => {
                let binding = match template.effort.as_deref() {
                    Some(effort) => format!("{}/{} · {effort}", template.account, template.model),
                    None => format!("{}/{}", template.account, template.model),
                };
                match presentation::tui_presentation_values(paths) {
                    Err(error) => product_doctor::RootTemplateStatus::Unknown {
                        detail: error.message().to_owned(),
                    },
                    Ok((_, sections)) => {
                        let credential = product_config::catalog_credential_readiness(
                            &template.account,
                            &sections,
                        );
                        match product_config::template_readiness(template, credential, &sections) {
                            product_config::TemplateReadiness::Ready => {
                                product_doctor::RootTemplateStatus::Ready {
                                    name: name.to_owned(),
                                    binding,
                                }
                            }
                            product_config::TemplateReadiness::NotReady(reason) => {
                                product_doctor::RootTemplateStatus::NotReady {
                                    name: name.to_owned(),
                                    binding,
                                    reason: reason.reason(),
                                }
                            }
                        }
                    }
                }
            }
        },
    };
    let spawn_templates = doctor_spawn_templates(paths, &selected);
    let state_home = services.home().state()?.to_path_buf();
    // The same offline classification `plak status` and model runs read.
    let mut accounts = PlakousApplication::for_accounts(state_home.clone());
    let mut credential = |provider| match accounts.as_mut() {
        Ok(accounts) => accounts.offline_status_row(provider),
        Err(failure) => Err(failure.message().to_owned()),
    };
    let report = product_doctor::diagnose(product_doctor::DoctorInputs {
        version: plakous::VERSION.to_owned(),
        paths: doctor_paths,
        config,
        product_database,
        subagents_database,
        runtime_database,
        runtime_store,
        retention_failure_at_ns: telemetry_store::TelemetryStore::unresolved_retention_failure(
            paths,
        )
        .ok()
        .flatten(),
        telemetry,
        working_directory: env::current_dir().map_err(|_| "cwd unavailable".to_owned()),
        terminals: product_doctor::TerminalFacts::capture(),
        composition_requirement: if local_composition_required {
            product_doctor::Requirement::Required
        } else {
            product_doctor::Requirement::Optional
        },
        composition_host,
        python_containment_required: selected_composition
            == product_config::ProductCompositionSelection::PyPtc,
        runtime_directory: env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        openrouter_catalog,
        root_template,
        spawn_templates,
        oauth: product_doctor::OauthCredentialProbes {
            codex: product_doctor::probe_oauth_credential(
                "codex",
                credential(NativeProvider::Codex),
                &state_home.join("plak/subscriptions/codex/credential.json"),
                telemetry_store::TelemetryStore::latest_credential_refresh(paths, "codex")
                    .map_err(|error| error.to_string()),
            ),
            xai: product_doctor::probe_oauth_credential(
                "xai",
                credential(NativeProvider::Xai),
                &state_home.join("plak/subscriptions/xai/credential.json"),
                telemetry_store::TelemetryStore::latest_credential_refresh(paths, "xai")
                    .map_err(|error| error.to_string()),
            ),
            claude: product_doctor::probe_oauth_credential(
                "claude",
                credential(NativeProvider::Claude),
                &state_home.join("plak/subscriptions/claude/credential.json"),
                telemetry_store::TelemetryStore::latest_credential_refresh(paths, "claude")
                    .map_err(|error| error.to_string()),
            ),
        },
    });

    writeln!(stdout, "Plak doctor\n").map_err(|_| CliFailure::new("stdout failed"))?;
    let mut table = cli_output::Table::new(["CHECK", "RESULT", "DETAIL", "FIX"]);
    for check in &report.checks {
        table
            .push([
                match check.kind {
                    product_doctor::CheckKind::OauthCodex => "OAuth codex".to_owned(),
                    product_doctor::CheckKind::OauthXai => "OAuth grok".to_owned(),
                    product_doctor::CheckKind::OauthClaude => "OAuth claude".to_owned(),
                    product_doctor::CheckKind::OpenRouterCatalog => "OpenRouter catalog".to_owned(),
                    _ => title_case_token(check.kind.key()),
                },
                check.severity.label().to_ascii_uppercase(),
                format!("{} · {}", check.result, check.detail),
                check.remediation.as_deref().unwrap_or("—").to_owned(),
            ])
            .map_err(CliFailure::new)?;
    }
    cli_human_output()
        .write_table(stdout, &table)
        .map_err(|_| CliFailure::new("stdout failed"))?;
    let legacy_warnings = home::state::report_legacy_state(paths, stdout)
        .map_err(|_| CliFailure::new("stdout failed"))?;
    let expiry_warnings = crate::review_expiry::write_warnings(stdout, services.now_unix_seconds())
        .map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(
        stdout,
        "\nResult: {} passed · {} warnings · {} failures\nProvider credentials: run `plak status`",
        report.summary.ok,
        report.summary.warnings + legacy_warnings + expiry_warnings,
        report.summary.errors
    )
    .map_err(|_| CliFailure::new("stdout failed"))?;
    if report.summary.errors > 0 {
        Err(CliFailure::classified(
            CliFailureKind::Agent,
            format!("doctor found {} blocking failures", report.summary.errors),
        ))
    } else {
        Ok(ProcessOutcome::Success)
    }
}

/// The role label and text `session show` prints for one transcript row;
/// rows without operator text are skipped.
/// Readiness of every template the root can spawn, from the same local
/// inputs as the root template check.
fn doctor_spawn_templates(
    paths: &home::paths::PlakPaths,
    config: &product_config::ProductConfig,
) -> product_doctor::SpawnTemplatesStatus {
    let names = roster_input::nested_spawns(config.working_set.names(), |name| {
        config
            .templates
            .get(name.as_str())
            .map(|template| template.spawns.clone())
            .unwrap_or_default()
    });
    let mut spawnable: Vec<&str> = names.iter().map(|(_, name)| name.as_str()).collect();
    spawnable.sort_unstable();
    spawnable.dedup();
    if spawnable.is_empty() {
        return product_doctor::SpawnTemplatesStatus::None;
    }
    let sections = match presentation::tui_presentation_values(paths) {
        Ok((_, sections)) => sections,
        Err(error) => {
            return product_doctor::SpawnTemplatesStatus::Unknown {
                detail: error.message().to_owned(),
            };
        }
    };
    let templates: Vec<_> = spawnable
        .iter()
        .filter_map(|name| {
            let template = config.templates.get(*name)?;
            let credential =
                product_config::catalog_credential_readiness(&template.account, &sections);
            let reason =
                product_config::template_readiness(template, credential, &sections).reason()?;
            Some(product_doctor::NotReadyTemplate {
                name: (*name).to_owned(),
                account: template.account.clone(),
                reason,
            })
        })
        .collect();
    if templates.is_empty() {
        product_doctor::SpawnTemplatesStatus::Ready {
            count: spawnable.len(),
        }
    } else {
        product_doctor::SpawnTemplatesStatus::NotReady { templates }
    }
}
