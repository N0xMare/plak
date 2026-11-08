//! Request-scoped picker reads share the host's process-owned runner probes.

use std::collections::BTreeSet;

use plakous_host_protocol::views::{ProfilesView, ReadyCatalog, TemplateRow};
use plakous_host_protocol::wire::{self, RequestRefusal};
use plakous_hosted::host_service::wire::PickerProgress;

use super::{ProductSessions, runner_views};
use crate::home::paths::PlakPaths;

fn unavailable(detail: impl Into<String>) -> RequestRefusal {
    RequestRefusal::SessionUnavailable {
        detail: detail.into(),
    }
}

struct Choices {
    profiles: ProfilesView,
    models: ReadyCatalog,
    templates: Vec<TemplateRow>,
}

fn choices(paths: &PlakPaths) -> Choices {
    let models = crate::presentation::tui_presentation_bundle(paths)
        .map(|bundle| bundle.sections)
        .unwrap_or_default();
    let profiles = match crate::product_config::ProductConfig::load(paths) {
        Ok(config) => crate::profiles::list(&config, paths.home()),
        Err(error) => ProfilesView {
            diagnostics: vec![format!(
                "templates could not be loaded because config.toml failed: {error}"
            )],
            ..ProfilesView::default()
        },
    };
    let templates = crate::agents_projection::template_rows(paths, &models.accounts);
    Choices {
        profiles,
        models,
        templates,
    }
}

impl ProductSessions {
    fn picker_row(
        &self,
        row: &plakous::runner::RunnerRow,
    ) -> Result<wire::RunnerOptions, RequestRefusal> {
        let mut recents = self
            .with_product(|store| store.recent_workspaces(&row.runner.runner_id))
            .map_err(unavailable)?
            .into_iter()
            .map(|(path, updated_at)| wire::RecentWorkspace { path, updated_at })
            .collect::<Vec<_>>();
        if row.runner.runner_id == "local" {
            let paths = self
                .services
                .paths()
                .map_err(|error| unavailable(error.message()))?;
            let catalog = plakous::runner::WorkspaceCatalog::local(None, &[], paths.home())
                .map_err(|_| {
                    unavailable("the product home is unavailable for local workspace discovery")
                })?;
            recents.retain(|recent| catalog.validate(&recent.path, true).is_ok());
        }
        runner_views::options(row, &self.services.runners().rows(), recents).map_err(unavailable)
    }

    fn refresh_options(
        &self,
        progress: &mut dyn FnMut(PickerProgress) -> Result<(), String>,
    ) -> Result<Vec<wire::RunnerOptions>, RequestRefusal> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| unavailable("the host runner executor is unavailable"))?;
        let registry = self.services.runners();
        let mut events = registry.progress_events();
        let mut completed = BTreeSet::new();
        let mut phases = BTreeSet::new();
        let rows = runtime.block_on(async {
            let refresh = registry.refresh(None);
            tokio::pin!(refresh);
            loop {
                tokio::select! {
                    result = &mut refresh => return result.map_err(|failure| {
                        runner_views::refusal(&wire::RunnerLabel::local(), failure, &registry.rows())
                    }),
                    event = events.recv() => match event {
                        Ok(event) => {
                            let rows = registry.rows();
                            if !rows.iter().any(|row| row.runner.runner_id == event.runner_id) {
                                continue;
                            }
                            let phase = runner_views::phase(event.phase);
                            let row = if let Some(row) = event.row {
                                if !completed.insert(event.runner_id.clone()) { continue; }
                                Some(self.picker_row(&row)?)
                            } else {
                                if completed.contains(&event.runner_id)
                                    || !phases.insert((event.runner_id.clone(), event.phase as u8)) {
                                    continue;
                                }
                                None
                            };
                            progress(PickerProgress {
                                runner: runner_views::label(&event.runner_id, &rows), phase, row,
                            }).map_err(unavailable)?;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err(unavailable("runner progress ended")),
                    }
                }
            }
        })?;
        let mut runners = Vec::with_capacity(rows.len());
        for row in rows {
            let row = self.picker_row(&row)?;
            if completed.insert(row.runner.runner_id.clone()) {
                progress(PickerProgress {
                    runner: row.runner.clone(),
                    phase: None,
                    row: Some(row.clone()),
                })
                .map_err(unavailable)?;
            }
            runners.push(row);
        }
        Ok(runners)
    }

    pub(super) fn session_options(
        &self,
        workspace: Option<&wire::WorkspaceProbe>,
        progress: &mut dyn FnMut(PickerProgress) -> Result<(), String>,
    ) -> Result<wire::SessionOptions, RequestRefusal> {
        let runners = self.refresh_options(progress)?;
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| unavailable("the host runner executor is unavailable"))?;
        let registry = self.services.runners();
        let home = PlakPaths::explicit(
            self.services
                .paths()
                .map_err(|error| unavailable(error.message()))?
                .home()
                .to_owned(),
        );
        let shared = choices(&home);
        let workspace = workspace
            .map(|probe| {
                match runtime.block_on(registry.validate_workspace(
                    &probe.runner.runner_id,
                    &probe.path,
                    false,
                )) {
                    Ok(canonical_path) => {
                        let context = if probe.runner.runner_id == "local" {
                            home.for_workspace(std::path::Path::new(&canonical_path))
                        } else {
                            home.for_remote_workspace(std::path::Path::new(&canonical_path))
                        };
                        let selected = choices(&context);
                        Ok(wire::WorkspaceOptions::Valid {
                            canonical_path,
                            profiles: selected.profiles,
                            models: selected.models,
                            templates: selected.templates,
                        })
                    }
                    Err(failure) => {
                        match runner_views::refusal(&probe.runner, failure, &registry.rows()) {
                            RequestRefusal::PlacementRefused { reason, .. } => {
                                Ok(wire::WorkspaceOptions::Refused { refusal: *reason })
                            }
                            other => Err(other),
                        }
                    }
                }
            })
            .transpose()?;
        Ok(wire::SessionOptions {
            runners,
            models: shared.models,
            profiles: shared.profiles,
            templates: shared.templates,
            workspace,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "isolated picker fixtures")]
mod tests;
