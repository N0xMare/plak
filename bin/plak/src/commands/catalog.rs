//! Authenticated model catalog listing (`plak models <account>`).

use std::io;

use plakous::subscriptions::NativeProvider;

use super::application::AccountOps;
use super::login::ensure_catalog_compatibility;
use super::support::{native_tool_mode_label, native_visibility_label, require_ready};
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};

/// Lists the authenticated catalog rows exactly as the provider admits them.
pub fn list_models(
    backend: &mut impl AccountOps,
    provider: NativeProvider,
    stdin: &mut dyn io::BufRead,
    stdout: &mut dyn io::Write,
    stderr: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    ensure_catalog_compatibility(backend, provider, stdin, stderr)?;
    let refreshed = backend.refresh(provider)?;
    require_ready(provider, refreshed)?;
    let models = backend.models(provider)?;
    if models.is_empty() {
        return Err(CliFailure::new(
            "authenticated native model catalog is empty",
        ));
    }
    for model in models {
        writeln!(
            stdout,
            "{}\tvisibility={}\ttool_mode={}\tstreaming={}\ttools={}\treasoning={}\tefforts={}\tcontext_length={}",
            model.exact_id,
            native_visibility_label(model.visibility),
            model.native_tool_mode.map_or("n/a", native_tool_mode_label),
            model.streaming,
            model.tools,
            !model.reasoning_efforts.is_empty(),
            if model.reasoning_efforts.is_empty() {
                "none".to_owned()
            } else {
                model
                    .reasoning_efforts
                    .iter()
                    .map(|effort| plakous_hosted::runtime::effort_label(*effort))
                    .collect::<Vec<_>>()
                    .join(",")
            },
            model
                .reported_context_length
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string())
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}

/// Lists OpenRouter catalog IDs; presence alone makes no capability claim.
pub fn list_openrouter_models(
    backend: &mut impl AccountOps,
    stdout: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    let models = backend.openrouter_models()?;
    for model in models {
        writeln!(stdout, "{model}").map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}

/// Lists one direct API route's models without an inference request: every
/// id the provider lists (live, cached or, offline, the bundled table), its
/// status, efforts, working and full window and output bound, and for a
/// usable row the compositions `platform::api_composition` admits.
pub fn list_platform_models(
    catalog: &plakous_hosted::platform::ApiModelCatalog,
    stdout: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    use plakous_hosted::binding::CompositionSelection;
    use plakous_hosted::platform::{ApiCatalogSource, ApiWindow, api_composition, effort_names};
    let write = |stdout: &mut dyn io::Write, line: String| {
        writeln!(stdout, "{line}").map_err(|_| CliFailure::new("stdout failed"))
    };
    let source = match &catalog.source {
        ApiCatalogSource::Live => "live list".to_owned(),
        ApiCatalogSource::Cached { age_seconds } => {
            format!("cached list, {} min old", age_seconds / 60)
        }
        ApiCatalogSource::Bundled { reason } => format!("bundled table ({reason})"),
    };
    write(
        stdout,
        format!("# {} models · {source}", catalog.provider.account()),
    )?;
    for row in &catalog.models {
        let efforts = effort_names(&row.efforts).join(",");
        let windows = format!(
            "window={}\tfull_window={}\tmax_output={}",
            row.window(ApiWindow::Working),
            row.window(ApiWindow::Full),
            row.output_bound()
        );
        if !row.usable() {
            write(
                stdout,
                format!(
                    "{}\t{}\tefforts={efforts}\t{windows}\treason={}",
                    row.model,
                    row.status.label(),
                    row.status.reason().unwrap_or_default()
                ),
            )?;
            continue;
        }
        let admitted = [
            CompositionSelection::Direct,
            CompositionSelection::NativePtc,
            CompositionSelection::JsPtc,
            CompositionSelection::PyPtc,
        ]
        .into_iter()
        .filter(|composition| api_composition(catalog.provider, &row.model, *composition).is_ok())
        .map(CompositionSelection::as_str)
        .collect::<Vec<_>>()
        .join(",");
        let default = api_composition(
            catalog.provider,
            &row.model,
            CompositionSelection::FollowCatalog,
        )
        .map_err(CliFailure::new)?;
        let deprecation = row
            .deprecation
            .as_deref()
            .map_or_else(String::new, |note| format!("\tnote={note}"));
        write(
            stdout,
            format!(
                "{}\tusable\tefforts={efforts}\t{windows}\tcomposition={admitted}\tplakous-default={}{deprecation}",
                row.model,
                default.as_str(),
            ),
        )?;
    }
    Ok(ProcessOutcome::Success)
}
