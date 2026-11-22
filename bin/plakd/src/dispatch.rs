//! Large `execute_command` arms, kept out of the crate root.

use std::io;

use crate::commands;
use crate::commands::application::AccountOps;
use crate::commands::session::{load_admitted_product_session, run_attached_action};
use crate::commands::support::config_set_failure;
use crate::commands::usage::{collect_usage_limits, usage_vertical_for_account};
use crate::presentation::{
    cli_human_output, event_table_row, format_unix_seconds, product_account_label,
    session_preview_line,
};
use crate::product_subscription_usage;
use crate::shell::services::Services;
use crate::surface_protocol;
use crate::surface_protocol::SurfaceIntent;
use crate::{
    CliFailure, CliFailureKind, CliResult, ProcessOutcome, carry, cli_output, product_config,
    product_session_log, store, topology_product,
};

pub(super) fn execute_config_show(
    services: &Services,
    stdout: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    let paths = services.paths()?;
    let cfg = product_config::ProductConfig::load(paths).map_err(CliFailure::from)?;
    write!(stdout, "{}", cfg.display_redacted(paths))
        .map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

pub(super) fn execute_config_set(
    services: &Services,
    stdout: &mut dyn io::Write,
    stderr: &mut dyn io::Write,
    key: String,
    value: String,
    scope: product_config::ConfigWriteScope,
) -> CliResult<ProcessOutcome> {
    let paths = services.paths()?;
    let mut cfg = product_config::ProductConfig::load(paths).map_err(CliFailure::from)?;
    let field = product_config::config_key(&key).map(|row| row.field);
    if scope == product_config::ConfigWriteScope::Project
        && field.is_some_and(product_config::ConfigField::is_home_only)
    {
        return Err(CliFailure::new(format!(
            "{key} is home-scoped; use --scope user to change orchestration authority"
        )));
    }
    if scope == product_config::ConfigWriteScope::User
        && let Some(field) = field
        && cfg.origin(field) == product_config::ConfigValueOrigin::Project
    {
        return Err(CliFailure::new(format!(
            "{key} is set by the project config {}; use --scope project to change it there",
            cfg.project
                .as_ref()
                .map(|source| source.root.join(".plak/config.toml").display().to_string())
                .unwrap_or_else(|| ".plak/config.toml".to_owned())
        )));
    }
    cfg.set_key(&key, &value).map_err(config_set_failure)?;
    if crate::config_key_changes_orchestration(&key) {
        store::revisions::refuse_incompatible(paths).map_err(CliFailure::new)?;
    }
    if scope == product_config::ConfigWriteScope::Project
        && let Some(field) = field
    {
        cfg.set_origin(field, product_config::ConfigValueOrigin::Project);
    }
    if scope == product_config::ConfigWriteScope::User {
        let mut warning = None;
        product_config::ProductConfig::update_user(paths, |current| {
            current.set_key(&key, &value)?;
            warning =
                crate::runtime_headroom::check_cap_edit(paths, &key, current.runtime_store_bounds)?;
            if crate::config_key_changes_orchestration(&key)
                && let Some(reason) = topology_product::roster_apply_refusal(paths, None)
            {
                return Err(reason.message().to_owned());
            }
            Ok(())
        })
        .map_err(|error| CliFailure::new(error.into_message()))?;
        if let Some(warning) = warning {
            writeln!(stderr, "plakd: warning: {warning}")
                .map_err(|_| CliFailure::new("stderr failed"))?;
        }
        let note = if crate::config_key_changes_orchestration(&key) {
            " · saved; applies to new sessions and explicit profile switches"
        } else {
            ""
        };
        writeln!(stdout, "set {key}{note}").map_err(|_| CliFailure::new("stdout failed"))?;
        return Ok(ProcessOutcome::Success);
    }
    if crate::config_key_changes_orchestration(&key) {
        // a roster or orchestration change is accepted only
        // from a quiescent tree. A headless `plakd config set` names no
        // session, so every persisted tree must be quiescent; the file
        // is then saved and a new session reads it at genesis.
        let outcome = topology_product::apply_unbound_roster(paths, &cfg, scope)
            .map_err(|error| CliFailure::new(error.into_message()))?;
        let disposition = match outcome {
            topology_product::RosterApplyOutcome::Saved => {
                "saved; applies to new sessions and explicit profile switches"
            }
            topology_product::RosterApplyOutcome::Applied
            | topology_product::RosterApplyOutcome::AppliedDurabilityUncertain(_) => "applied",
        };
        writeln!(stdout, "set {key} · {disposition}")
            .map_err(|_| CliFailure::new("stdout failed"))?;
        return Ok(ProcessOutcome::Success);
    }
    cfg.save_scoped(paths, scope)
        .map_err(|error| CliFailure::new(error.into_message()))?;
    let scope_note = if scope == product_config::ConfigWriteScope::Project {
        " · project"
    } else {
        ""
    };
    writeln!(stdout, "set {key}{scope_note}").map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

pub(super) fn execute_session_list(
    services: &Services,
    stdout: &mut dyn io::Write,
    include_archived: bool,
) -> CliResult<ProcessOutcome> {
    let paths = services.paths()?;
    let cwd = paths.workspace().map_err(CliFailure::new)?.to_path_buf();
    let sessions = store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?
        .list_for_workspace_filtered(&cwd, include_archived)
        .map_err(CliFailure::new)?;
    if sessions.is_empty() {
        writeln!(stdout, "No sessions in this workspace.")
            .map_err(|_| CliFailure::new("stdout failed"))?;
    } else {
        writeln!(stdout, "Sessions in {}", cwd.display())
            .map_err(|_| CliFailure::new("stdout failed"))?;
        writeln!(stdout).map_err(|_| CliFailure::new("stdout failed"))?;
        let mut table = cli_output::Table::new([
            "SESSION ID",
            "UPDATED",
            "PROVIDER",
            "MODEL",
            "EVENTS",
            "TITLE",
        ]);
        for s in sessions {
            let n = product_session_log::count_events(&paths.session_home(), &s.id)
                .map_or_else(|_| "—".to_owned(), |count| count.to_string());
            let preview = match carry::carried_forward_label(&s) {
                Some(link) => format!("{} [{link}]", session_preview_line(&s)),
                None => session_preview_line(&s),
            };
            let preview = if s.archived {
                format!("[archived] {preview}")
            } else {
                preview
            };
            table
                .push([
                    s.id,
                    format_unix_seconds(s.updated_at_unix),
                    product_account_label(s.account.as_deref()),
                    s.model.unwrap_or_else(|| "—".to_owned()),
                    n,
                    preview,
                ])
                .map_err(CliFailure::new)?;
        }
        cli_human_output()
            .write_table(stdout, &table)
            .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}

pub(super) fn execute_session_log(
    services: &Services,
    stdout: &mut dyn io::Write,
    id: String,
    kind: Option<String>,
    tail: usize,
    json: bool,
) -> CliResult<ProcessOutcome> {
    let paths = services.paths()?;
    if id.contains('.') {
        commands::session::log_child_read_only(paths, &id, kind.as_deref(), tail, json, stdout)?;
        return Ok(ProcessOutcome::Success);
    }
    let id = commands::session::resolve_read_only_session_id(paths, &id)?;
    let _ = store::ProductStore::inspect_session_read_only(paths, &id).map_err(CliFailure::from)?;
    let lines =
        product_session_log::read_tail_for_kind(&paths.session_home(), &id, tail, kind.as_deref())
            .map_err(CliFailure::from)?;
    if json {
        for line in lines {
            writeln!(stdout, "{line}").map_err(|_| CliFailure::new("stdout failed"))?;
        }
    } else if lines.is_empty() {
        writeln!(stdout, "No observational events recorded for this session.")
            .map_err(|_| CliFailure::new("stdout failed"))?;
    } else {
        let mut table = cli_output::Table::new(["TIME", "SEQ", "KIND", "DETAILS"]);
        for line in &lines {
            let value: serde_json::Value = serde_json::from_str(line)
                .map_err(|_| CliFailure::new("session event is invalid"))?;
            table
                .push(event_table_row(&value))
                .map_err(CliFailure::new)?;
        }
        cli_human_output()
            .write_table(stdout, &table)
            .map_err(|_| CliFailure::new("stdout failed"))?;
        writeln!(
            stdout,
            "\nEvents are observational and are not recovery authority."
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}

pub(super) fn execute_usage(
    services: &Services,
    backend: &mut impl AccountOps,
    stdout: &mut dyn io::Write,
    account: Option<plakous_hosted::account::AccountRef>,
    session_id: Option<String>,
) -> CliResult<ProcessOutcome> {
    if let Some(session_id) = session_id {
        let record = load_admitted_product_session(
            services.paths()?,
            services.session_records()?.as_ref(),
            &session_id,
        )?;
        let account = record.account.clone();
        run_attached_action(
            services,
            record,
            surface_protocol::UserAction::Extension(SurfaceIntent::OpenUsage),
            stdout,
        )?;
        crate::write_selected_subscription_usage(
            account.as_deref(),
            backend,
            services.paths().ok(),
            stdout,
        )?;
        return Ok(ProcessOutcome::Success);
    }
    let selected = match account
        .as_ref()
        .map(plakous_hosted::account::AccountRef::as_str)
    {
        None => None,
        Some(account) => match usage_vertical_for_account(account) {
            Some(vertical) => Some(vertical),
            None => {
                return Err(CliFailure::classified(
                    CliFailureKind::Usage,
                    "usage account must be codex|grok|claude|openrouter|supergrok",
                ));
            }
        },
    };
    let view = collect_usage_limits(selected, backend)?;
    product_subscription_usage::write_limits(
        &view,
        services.paths().ok(),
        crate::local_time_zone(),
        stdout,
    )
    .map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(
        stdout,
        "\nTurn tokens and context capacity require `plakd usage --session <uuid>`; they are not subscription allowance."
    )
    .map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}
